//! The local socket front ends connect to.
//!
//! One task per connection, each owning its own writer. Nothing here mutates
//! daemon state directly: requests go to the session, changes come back
//! through [`StateHub`], which is what keeps two clients from racing each
//! other into an inconsistent view.

#[cfg(not(target_family = "wasm"))]
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use oxidezap_ipc::{
    CallAction, ClientRequest, DaemonMessage, PROTOCOL_VERSION, ProtocolError, Request, RequestId,
};
#[cfg(not(target_family = "wasm"))]
use oxidezap_ipc::{endpoint_path, lock_path, state_dir};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt, BufReader, ReadHalf,
    WriteHalf,
};
use tokio::sync::broadcast::error::RecvError;

#[cfg(not(target_family = "wasm"))]
use crate::listener::Listener;
use crate::session_bridge::{Action, CommandOutcome, Commands, Outbox, SessionCommand};
use crate::state::StateHub;

#[cfg(not(target_family = "wasm"))]
/// This process's claim on being *the* daemon for this user.
///
/// Taken before anything touches the account. Holding it is what makes a
/// second daemon fail fast instead of racing the first.
pub struct Claim {
    path: PathBuf,
    _lock: StartupLock,
}

#[cfg(not(target_family = "wasm"))]
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
    let path = endpoint_path().context("no per-user directory to place the endpoint in")?;
    let dir = state_dir().context("no per-user directory for the daemon's own state")?;
    prepare_state_dir(&dir)?;
    let lock = acquire_startup_lock(&lock_path().context("no per-user directory for the lock")?)?;
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

/// The admission cap, shared by every transport that serves front ends.
///
/// One count across all of them, not one each: the descriptors, the tasks and
/// the per-connection buffers come out of the same process however a client
/// arrived. A second endpoint with a cap of its own would double what a
/// reconnect loop can hold open.
pub type ClientSlots = Arc<tokio::sync::Semaphore>;

/// A fresh set of them.
#[must_use]
pub fn client_slots() -> ClientSlots {
    Arc::new(tokio::sync::Semaphore::new(MAX_CLIENTS))
}

/// How many frames may queue for one connection's own answers.
///
/// Only answers to requests land here, and a front end asks for as many as it
/// has visible media. Past this the frame is *not* dropped: see
/// `session_bridge::answer_now`, which hands a full outbox to a task that
/// waits on the connection's own writer, because a dropped answer leaves the
/// view that asked waiting forever and it never asks again.
///
/// The price is that answers past this point are delivered by request id and
/// not in order: a frame parked on a full outbox can be overtaken by the next
/// one, if that one fits. Every answer names the `RequestId` it belongs to, so
/// nothing is lost, but two pages of one paged `LoadMessages` can arrive the
/// wrong way round.
const OUTBOX_CAPACITY: usize = 64;

#[cfg(not(target_family = "wasm"))]
/// Serve until the future is dropped.
///
/// Borrows the claim rather than taking it: this future is a `select!` branch
/// and can be dropped while the session is still disconnecting, and the lock
/// has to outlive that. Handing it over here would release it mid-teardown,
/// which is exactly the window a second daemon must not find open.
pub async fn run(
    claim: &Claim,
    hub: Arc<StateHub>,
    plugins: Arc<oxidezap_plugin_host::Plugins>,
    commands: Commands,
    slots: ClientSlots,
) -> Result<()> {
    let path = claim.path.clone();
    let mut listener = Listener::bind(&path)?;
    log::info!("listening on {}", path.display());

    loop {
        let stream = match listener.accept().await {
            Ok(stream) => stream,
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
                oxidezap_session::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
            Err(e) => return Err(e).context("accepting a client"),
        };

        let Ok(slot) = Arc::clone(&slots).try_acquire_owned() else {
            tokio::spawn(reject(stream));
            continue;
        };

        let hub = Arc::clone(&hub);
        let plugins = Arc::clone(&plugins);
        let commands = commands.clone();
        // Per-connection task: one slow or malformed client cannot hold up
        // the accept loop or any other client.
        tokio::spawn(async move {
            if let Err(e) = serve_client(stream, hub, plugins, commands).await {
                log::debug!("client disconnected: {e}");
            }
            drop(slot);
        });
    }
}

#[cfg(not(target_family = "wasm"))]
/// Whether an `accept` failure describes one connection rather than the
/// listener.
fn is_transient_accept_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::Interrupted | ErrorKind::WouldBlock
    ) || matches!(e.raw_os_error(), Some(EMFILE | ENFILE))
}

#[cfg(not(target_family = "wasm"))]
/// Out of descriptors, for this process and for the machine. Spelled out
/// because neither has an `std::io::ErrorKind`: both land in
/// `Uncategorized`, which is unstable to match on.
const EMFILE: i32 = 24;
#[cfg(not(target_family = "wasm"))]
const ENFILE: i32 = 23;

#[cfg(not(target_family = "wasm"))]
/// Tell a client we are full, then close.
///
/// Public because the web bridge refuses the same way and for the same
/// reason: a refused client should learn why rather than watch its
/// connection drop.
///
/// Spawned rather than written inline: the accept loop must not wait on a
/// peer. The task is still bounded — one small frame into a socket nobody has
/// had a chance to fill, then done — so a refused client costs a write, not a
/// slot.
/// The refusal, as a frame, for a transport that has to deliver it itself.
///
/// The socket listener writes it onto the stream; the web bridge has to
/// complete a WebSocket upgrade first, so it needs the frame rather than the
/// writing.
///
/// # Errors
///
/// The frame could not be serialized.
pub(crate) fn too_many_clients_frame() -> Result<String> {
    error_frame(None, ProtocolError::TooManyClients { limit: MAX_CLIENTS })
}

#[cfg(not(target_family = "wasm"))]
pub(crate) async fn reject<S: AsyncRead + AsyncWrite + Send + 'static>(stream: S) {
    log::warn!("refusing a client: already serving {MAX_CLIENTS}");
    let (_, mut writer) = tokio::io::split(stream);
    if let Ok(frame) = error_frame(None, ProtocolError::TooManyClients { limit: MAX_CLIENTS }) {
        let _ = write_line(&mut writer, &frame).await;
    }
}

#[cfg(not(target_family = "wasm"))]
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
fn acquire_startup_lock(path: &Path) -> Result<StartupLock> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
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

/// The same exclusion without `flock`, which Windows does not have.
///
/// Opening with no sharing is the platform's own way to say "only me": a
/// second daemon's open fails while the first holds the handle, and the
/// kernel closes it however the first dies — which is the property the lock
/// was chosen for.
#[cfg(windows)]
fn acquire_startup_lock(path: &Path) -> Result<StartupLock> {
    use std::os::windows::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .share_mode(0)
        .open(path)
        .map_err(|e| {
            anyhow::anyhow!(
                "another daemon holds {} ({e}); refusing to start a second session",
                path.display()
            )
        })?;
    Ok(StartupLock { _file: file })
}

#[cfg(all(not(any(unix, windows)), not(target_family = "wasm")))]
fn acquire_startup_lock(_path: &Path) -> Result<StartupLock> {
    anyhow::bail!("no way to take a startup lock on this platform")
}

#[cfg(not(target_family = "wasm"))]
/// Make the directory the socket lives in ours alone.
///
/// The socket carries control of a WhatsApp session, and under the `TMPDIR`
/// fallback its directory sits at a predictable path in a world-writable
/// place. The check itself is shared with the media cache next door; what is
/// specific here is what a directory that *was* open means: another local
/// account could have left something inside under a name this daemon is about
/// to use — a `daemon.sock` in front of the bind, a `daemon.lock` held open,
/// a `media` symlink pointing at a directory of their own. Refusing to start
/// is a bad outcome; opening the account's photo cache through somebody
/// else's symlink is a worse one, so what could not be ours is removed rather
/// than inherited.
fn prepare_state_dir(dir: &Path) -> Result<()> {
    if crate::private_dir::prepare(dir, "the socket")? == crate::private_dir::Found::WasOpen {
        crate::private_dir::drop_foreign_entries(dir)?;
    }
    Ok(())
}

/// The daemon's half of [`oxidezap_ipc::read_frame`].
///
/// Not that function: this side reads inside a runtime, from an `AsyncRead`
/// it selects over, where a front end parks a thread in a blocking read. The
/// bound, the outcome and the resynchronization rules come from the ipc crate
/// so the two ends cannot disagree; only the loop is a platform each.
///
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
async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    buf: &mut Vec<u8>,
) -> Result<Option<oxidezap_ipc::FrameRead>> {
    // What is already here is a prefix a cancelled call left behind, and it
    // counts against this frame's budget: the cap is per frame, and a frame
    // read across three cancellations is still one frame.
    let carried = buf.len();
    if carried >= oxidezap_ipc::MAX_REQUEST_BYTES {
        buf.clear();
        return Ok(Some(oxidezap_ipc::FrameRead::TooLong));
    }

    let read = {
        let mut limited = reader.take((oxidezap_ipc::MAX_REQUEST_BYTES - carried) as u64);
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
        let hit_the_cap = buf.len() == oxidezap_ipc::MAX_REQUEST_BYTES;
        buf.clear();
        return Ok(hit_the_cap.then_some(oxidezap_ipc::FrameRead::TooLong));
    }

    buf.pop();
    let frame = match std::str::from_utf8(buf) {
        Ok(line) => oxidezap_ipc::FrameRead::Line(line.to_string()),
        Err(_) => oxidezap_ipc::FrameRead::NotUtf8,
    };
    // Cleared here, at the end of a complete frame, rather than at the start
    // of the next call: only a cancelled read may leave anything behind.
    buf.clear();
    Ok(Some(frame))
}

/// One front end, from the handshake to the close.
///
/// Generic over the stream, and reached from two places: the local endpoint's
/// accept loop, and the web bridge — which hands it one end of an in-process
/// duplex and moves the lines across a WebSocket. Everything about the
/// protocol lives here, so the second transport adds no second copy of it.
///
/// # Errors
///
/// The connection ended, or the peer said something unrecoverable.
pub(crate) async fn serve_client<S>(
    stream: S,
    hub: Arc<StateHub>,
    plugins: Arc<oxidezap_plugin_host::Plugins>,
    commands: Commands,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
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
    let attached = match oxidezap_session::with_timeout(
        handshake(&mut reader, &mut writer, &mut buf),
        HANDSHAKE_TIMEOUT,
    )
    .await
    {
        Some(result) => match result? {
            Some(attached) => attached,
            None => return Ok(()),
        },
        None => {
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
    // Only for a client that asked. The count of these receivers is what
    // tells the bridge whether to prepare session events at all — writing
    // every photo in the account to the cache and serializing its whole
    // traffic — so a tray holding one it never reads would make it do all of
    // that for nobody. An `Option` rather than a receiver whose sender is
    // already gone, because that looks exactly like a closed channel and the
    // branch below ends the connection on one.
    let mut sessions = attached.session_events.then(|| hub.subscribe_sessions());
    // Gated on having a window, not on wanting events. This is the one
    // channel whose cost is measured in megabits: a notifier or a tray asks
    // for session events and has nowhere to put a picture, and subscribing it
    // would spend a call's whole bitrate serializing frames it parses and
    // throws away — while delaying the events it did ask for. The count of
    // these receivers is also what tells the session whether to publish at
    // all, so a client that draws nothing must not hold one.
    let mut video = attached.has_window.then(|| hub.subscribe_video());
    if video.is_some() {
        // A subscriber that arrives mid-call starts wherever the stream is,
        // which is a P-frame referencing units published before it was
        // listening — so its decoders draw nothing until the encoder's own
        // periodic IDR, seconds after somebody opened a window to look. Asked
        // for here, where the subscription is, rather than left to the
        // session: this is the moment a decoder is born, and the session has
        // no way to see it happen. Nothing waits on the answer — there is no
        // call to ask about most of the time, and a keyframe that cannot be
        // requested changes nothing about serving this client.
        let _ = dispatch(&hub, &commands, Action::RefreshVideo).await;
    }

    // Held for the connection's whole life, so the count falls again however
    // this task ends. What it answers is "is there a window to raise": see
    // `crate::window::show`.
    let _window = attached.has_window.then(|| hub.attach_window());

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
            //
            // The reverse starvation is bounded rather than prevented: a
            // request is read only once every branch above it is `Pending`,
            // so a call with video or a hydration burst delays one. It cannot
            // accumulate, because every producer above is slower than a local
            // socket write is: a camera leaves tens of milliseconds between
            // frames and a burst is finite, so the loop empties them and
            // reaches the read. The delay is a frame interval, not a
            // conversation, and a `Hangup` waiting that long is waiting
            // less than the stanza it turns into.
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
            // The guard is what makes the `expect` safe: `select!` evaluates
            // it before the future.
            session = async { sessions.as_mut().expect("guarded").recv().await },
                if sessions.is_some() => match session {
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

            // Lossy on purpose, and the only branch that is. A video frame
            // carries no version and nothing recovers it, but unlike a window
            // request it is *worthless* a moment later: a client that fell
            // behind wants the newest frame, not the backlog, and telling it
            // to resync would throw its whole history away to catch up on a
            // picture that has already moved on.
            picture = async { video.as_mut().expect("guarded").recv().await },
                if video.is_some() => match picture {
                Ok(frame) => write_line(&mut writer, &frame).await?,
                Err(RecvError::Lagged(missed)) => {
                    log::trace!("client missed {missed} video frames");
                    // Said out loud, unlike a state gap: the client's decoder
                    // is holding references to units it will never get, and
                    // the frames that follow are built on them. It has no
                    // other way to know — what did not arrive leaves nothing
                    // behind to notice.
                    let frame = serde_json::to_string(&DaemonMessage::CallVideoGap)?;
                    write_line(&mut writer, &frame).await?;
                    // And asked for a point it can start from. Telling the
                    // decoders to stop is half an answer: what they hold is
                    // useless either way, and without this the picture stays
                    // blank until the encoder's own periodic IDR. Only our
                    // own camera can be asked — the peer's direction has
                    // nobody on this side to ask.
                    let _ = dispatch(&hub, &commands, Action::RefreshVideo).await;
                }
                Err(RecvError::Closed) => return Ok(()),
            },

            // Cancellation-safe: `read_frame` carries a partial frame in
            // `buf` across losing this race. See its documentation.
            frame = read_frame(&mut reader, &mut buf) => match frame? {
                Some(oxidezap_ipc::FrameRead::Line(line)) => {
                    // Parsed once, here: gating update delivery and answering
                    // are two decisions about one frame, and reading it twice
                    // is how they drift apart.
                    let request: Request = match serde_json::from_str(&line) {
                        Ok(request) => request,
                        Err(e) => {
                            // The client's bug, not a reason to drop the
                            // connection: it gets told and the stream stays
                            // usable. Without an id, because the frame that
                            // would have carried one is the one that did not
                            // parse.
                            write_line(&mut writer, &malformed(&e.to_string())?).await?;
                            continue;
                        }
                    };

                    if matches!(request.request, ClientRequest::Snapshot) {
                        // Resubscribe BEFORE snapshotting, the same ordering
                        // the connection opened with. Reusing the old receiver
                        // would leave it at the cursor that already lagged, so
                        // a client recovering during heavy traffic would lag
                        // again immediately on events the new snapshot already
                        // covers, and loop through `Resync` forever.
                        updates = hub.subscribe();
                        awaiting_resync = false;
                    }
                    let answer = handle_request(request, &hub, &plugins, &commands, &outbox).await;
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
                Some(oxidezap_ipc::FrameRead::NotUtf8) => {
                    write_line(&mut writer, &not_utf8()?).await?;
                }
                Some(oxidezap_ipc::FrameRead::TooLong) => {
                    // Unlike the other two this ends the connection: with no
                    // newline there is no way to know where the frame was meant
                    // to end, so the stream cannot be resynchronized.
                    let frame = malformed(&format!(
                    "frame exceeded {} bytes",
                    oxidezap_ipc::MAX_REQUEST_BYTES
                ))?;
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
async fn handshake<S: AsyncRead + AsyncWrite>(
    reader: &mut BufReader<ReadHalf<S>>,
    writer: &mut WriteHalf<S>,
    buf: &mut Vec<u8>,
) -> Result<Option<Attached>> {
    loop {
        match read_frame(reader, buf).await? {
            Some(oxidezap_ipc::FrameRead::Line(line)) => match check_hello(&line) {
                Ok(attached) => return Ok(Some(attached)),
                Err(rejection) => {
                    if let Some(rejection) = rejection {
                        write_line(writer, &rejection).await?;
                    }
                    return Ok(None);
                }
            },
            Some(oxidezap_ipc::FrameRead::NotUtf8) => write_line(writer, &not_utf8()?).await?,
            Some(oxidezap_ipc::FrameRead::TooLong) => {
                let frame = malformed(&format!(
                    "frame exceeded {} bytes",
                    oxidezap_ipc::MAX_REQUEST_BYTES
                ))?;
                write_line(writer, &frame).await?;
                return Ok(None);
            }
            None => return Ok(None),
        }
    }
}

/// An error frame, naming the request it answers when there is one.
fn error_frame(id: Option<RequestId>, error: ProtocolError) -> Result<String> {
    Ok(serde_json::to_string(&DaemonMessage::Error { id, error })?)
}

/// The answer to send when there is no answer left to encode.
///
/// Every request gets exactly one answer, the ones that fail included, and
/// `serde_json` failing is not an exception to that: dropping the frame
/// leaves the view that asked waiting on it forever, with nothing logged.
/// This value is a fixed shape with nothing in it that can fail to encode,
/// and the literal behind it is the same one the bridge falls back to.
fn unanswerable(id: Option<RequestId>, detail: &str) -> String {
    log::warn!("a daemon answer could not be encoded: {detail}");
    error_frame(
        id,
        ProtocolError::Malformed {
            detail: "the answer could not be encoded".to_string(),
        },
    )
    .unwrap_or_else(|_| r#"{"type":"error","error":"malformed","detail":"unanswerable"}"#.into())
}

/// One answer, whatever happened to the encoder.
fn always(id: Option<RequestId>, frame: Result<String>) -> Option<String> {
    Some(frame.unwrap_or_else(|e| unanswerable(id, &e.to_string())))
}

fn malformed(detail: &str) -> Result<String> {
    error_frame(
        None,
        ProtocolError::Malformed {
            detail: detail.into(),
        },
    )
}

fn not_utf8() -> Result<String> {
    malformed("frame was not valid UTF-8")
}

/// What an accepted hello asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Attached {
    /// Whether this client wants the session's own events as well as
    /// summaries. See [`ClientRequest::Hello`].
    session_events: bool,
    /// Whether this client owns a window. See [`ClientRequest::Hello`].
    has_window: bool,
}

/// Validate the client's opening frame.
///
/// `Err` carries the rejection to send; `Ok` carries what the client asked to
/// be served.
fn check_hello(line: &str) -> Result<Attached, Option<String>> {
    let Request { id, request } = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return Err(always(None, malformed(&e.to_string()))),
    };

    match request {
        ClientRequest::Hello {
            protocol,
            session_events,
            has_window,
        } if protocol == PROTOCOL_VERSION => Ok(Attached {
            session_events,
            has_window,
        }),
        ClientRequest::Hello { protocol, .. } => Err(always(
            id,
            error_frame(
                id,
                ProtocolError::VersionMismatch {
                    client: protocol,
                    daemon: PROTOCOL_VERSION,
                },
            ),
        )),
        _ => Err(always(None, malformed("first frame must be a hello"))),
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
    Request { id, request }: Request,
    hub: &StateHub,
    plugins: &Arc<oxidezap_plugin_host::Plugins>,
    commands: &Commands,
    outbox: &Outbox,
) -> Answer {
    // Every arm below answers under `id`, which is what lets a client match
    // an answer to the thing it asked — and why nothing here has to invent a
    // way to report a failure against the message a client happened to draw.
    let acted = |result| Answer::frame(answer(id, result));

    match request {
        ClientRequest::Snapshot => {
            Answer::frame(always(id, hub.hello_frame().map_err(anyhow::Error::from)))
        }
        // A second hello is harmless but says nothing; acknowledging keeps the
        // rule that every request gets exactly one answer.
        ClientRequest::Hello { .. } => acted(Ok(())),
        ClientRequest::SendText {
            jid,
            text,
            local_id,
            quoted,
        } => acted(
            dispatch(
                hub,
                commands,
                Action::SendText {
                    jid,
                    text,
                    local_id,
                    quoted,
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
            quoted,
        } => acted(
            dispatch(
                hub,
                commands,
                Action::SendAudio {
                    jid,
                    upload,
                    duration_secs,
                    waveform,
                    local_id,
                    quoted,
                },
            )
            .await,
        ),
        ClientRequest::Typing { jid, composing } => {
            acted(dispatch(hub, commands, Action::Typing { jid, composing }).await)
        }
        ClientRequest::Call(action) => acted(dispatch(hub, commands, Action::Call(action)).await),
        ClientRequest::Download { media } => {
            // The only request that needs an id rather than merely carrying
            // one: its answer arrives seconds later, on a channel shared with
            // every other download this client asked for.
            let Some(id) = id else {
                return Answer::frame(always(
                    None,
                    error_frame(
                        None,
                        ProtocolError::Malformed {
                            detail: "a download needs an id to answer under".into(),
                        },
                    ),
                ));
            };
            // The one request whose answer is *not* an acknowledgement. It
            // comes back as `Downloaded` under this id, seconds later, from
            // the task the action spawns — so acknowledging it here would be
            // a second answer under the same id, and a client that took its
            // waiter off the first one would drop the bytes when they arrived.
            // Only a refusal is answered here, because then nothing else will.
            match dispatch(
                hub,
                commands,
                Action::Download {
                    id,
                    media,
                    answer_to: outbox.clone(),
                },
            )
            .await
            {
                Ok(()) => Answer::frame(None),
                Err(error) => Answer::frame(always(Some(id), error_frame(Some(id), error))),
            }
        }
        ClientRequest::ReloadHistory => acted(dispatch(hub, commands, Action::ReloadHistory).await),
        // Answered with the page under this id, like a download and for the
        // same reason: the rows are the answer rather than an
        // acknowledgement, so only a refusal is answered here.
        ClientRequest::LoadMessages { jid, before, limit } => {
            let Some(id) = id else {
                return Answer::frame(always(
                    None,
                    error_frame(
                        None,
                        ProtocolError::Malformed {
                            detail: "a page needs an id to answer under".into(),
                        },
                    ),
                ));
            };
            match dispatch(
                hub,
                commands,
                Action::LoadMessages {
                    id,
                    jid,
                    before,
                    limit,
                    answer_to: outbox.clone(),
                },
            )
            .await
            {
                Ok(()) => Answer::frame(None),
                Err(error) => Answer::frame(always(Some(id), error_frame(Some(id), error))),
            }
        }
        ClientRequest::LoadChats { after, limit } => {
            let Some(id) = id else {
                return Answer::frame(always(
                    None,
                    error_frame(
                        None,
                        ProtocolError::Malformed {
                            detail: "a page needs an id to answer under".into(),
                        },
                    ),
                ));
            };
            match dispatch(
                hub,
                commands,
                Action::LoadChats {
                    id,
                    after,
                    limit,
                    answer_to: outbox.clone(),
                },
            )
            .await
            {
                Ok(()) => Answer::frame(None),
                Err(error) => Answer::frame(always(Some(id), error_frame(Some(id), error))),
            }
        }
        ClientRequest::ForgetSession => acted(dispatch(hub, commands, Action::ForgetSession).await),
        ClientRequest::MarkRead {
            jid,
            through_message_id,
        } => acted(
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
        ClientRequest::MarkStatusWatched { message_ids } => {
            acted(dispatch(hub, commands, Action::MarkStatusWatched { message_ids }).await)
        }
        // Measured here rather than by the client: the daemon is the only
        // process that opens the store or writes the media cache, so a front
        // end asking the filesystem would be guessing at paths it does not
        // own. No session needed — this is two directory reads.
        ClientRequest::StorageUsage => {
            // Answered under an id like a download, because the numbers are
            // the answer rather than an acknowledgement of it.
            let Some(id) = id else {
                return Answer::frame(always(
                    None,
                    error_frame(
                        None,
                        ProtocolError::Malformed {
                            detail: "a storage query needs an id to answer under".into(),
                        },
                    ),
                ));
            };
            // Two directory walks, off the runtime for the same reason the
            // clear is.
            let measured = oxidezap_session::unblock(|| {
                let (media_bytes, media_files) = crate::media::cache_usage();
                (database_bytes(), media_bytes, media_files)
            })
            .await;
            let (database_bytes, media_bytes, media_files) = measured.unwrap_or((0, 0, 0));
            Answer::frame(always(
                Some(id),
                serde_json::to_string(&DaemonMessage::Storage {
                    id,
                    database_bytes,
                    media_bytes,
                    media_files,
                })
                .map_err(anyhow::Error::from),
            ))
        }
        // The store stays; every message keeps its `downloadable`, so what
        // this costs is a re-download of whatever is looked at again.
        ClientRequest::ClearMediaCache => {
            // Off the runtime, for the reason the plugin approval is: this
            // reads a directory of up to half a gigabyte and deletes it file
            // by file, holding a lock that the session's own publish thread
            // takes for every photo it caches. Done here it stopped event
            // delivery for as long as a slow disk took. Awaited rather than
            // spawned loose, so the acknowledgement still means the cache is
            // clear.
            let cleared = oxidezap_session::unblock(|| {
                // Cached downloads only: a staged upload belongs to a send
                // that has not run yet. See `media::Wipe`.
                crate::media::wipe(crate::media::Wipe::Cache).map_err(|e| e.to_string())
            })
            .await;
            acted(match cleared {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(ProtocolError::Malformed {
                    detail: format!("could not clear the media cache: {e}"),
                }),
                Err(_) => Err(ProtocolError::Malformed {
                    detail: "the media cache was not cleared".to_string(),
                }),
            })
        }
        // The daemon has no window of its own, so this is relayed rather than
        // acted on: whoever owns a window is the only one that can raise it.
        // Published to every client, including the one that asked, because a
        // front end that sent this on a user's behalf wants the window up
        // regardless of which process is holding it. Through the same door as
        // the tray's Open, so that "there should be a window" means the same
        // thing however it was asked — including when there is none to raise.
        ClientRequest::ShowWindow => {
            crate::window::show(hub);
            acted(Ok(()))
        }
        // Not dispatched to the session: a plugin action touches the account
        // only if the plugin decides it should, and what it decides is its
        // own business. Handing it over is the whole of the daemon's part,
        // which is why this answers `Accepted` rather than waiting — the
        // plugin's own answer reaches it inside the sandbox, where a socket
        // front end's never could.
        ClientRequest::PluginAction { action } => {
            plugins.act(&action);
            acted(Ok(()))
        }
        // The one thing about a plugin that a plugin has no say in. Answered
        // rather than dispatched, like the action above: what the plugin does
        // with its new permissions is its own business and arrives as a
        // republished surface.
        ClientRequest::PluginApproval { plugin, approved } => {
            // Where it is recorded is `plugins::approve`, which is a platform
            // split: a desktop writes and renames a file and so must leave the
            // runtime's thread, and a page writes `localStorage` and has no
            // blocking pool to leave for — `spawn_blocking` here panicked
            // outright in a browser, so approving a plugin there never worked.
            // Awaited either way, so the acknowledgement still means the
            // answer is recorded.
            let recorded = crate::plugins::approve(plugins, plugin, approved).await;
            if recorded {
                acted(Ok(()))
            } else {
                acted(Err(ProtocolError::Refused {
                    detail: "the approval could not be recorded".to_string(),
                }))
            }
        }
        // Answered when it has happened, not when it was taken: a front end
        // draws "done" from the acknowledgement, and the set that came back
        // travels beside it as ordinary state — every other window learns of
        // it the same way, because a plugin's interface was always the
        // daemon's rather than the asking window's.
        ClientRequest::ReloadPlugins => {
            let running = crate::plugins::reload(plugins).await;
            log::info!("plugins reloaded: {running} running");
            acted(Ok(()))
        }
        // The acknowledgement goes out first; see the caller.
        ClientRequest::Shutdown => Answer {
            frame: answer(id, Ok(())),
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
async fn dispatch(
    hub: &StateHub,
    commands: &Commands,
    action: Action,
) -> Result<(), ProtocolError> {
    // Refused early as well as late: a client that is watching the connection
    // state should get the answer it can already predict, without the round
    // trip. Only for what actually needs the network — see
    // [`Action::needs_network`].
    let connection = hub.connection();
    if action.needs_network() && !connection.is_connected() {
        // A call the asking window already drew has to be un-drawn. It
        // passed its own connection check before this one moved, the refusal
        // rides no request id, and nothing on that side connects the error
        // back to the stage it is holding — so the stage would sit there
        // until the next snapshot dropped it, and disappearing is what a
        // front end writes down as an attempt that was never answered. The
        // bridge's busy refusal says the same thing one layer down.
        if let Action::Call(CallAction::Start { placeholder_id, .. }) = &action {
            hub.calls(|calls| calls.mark_unrecorded(placeholder_id));
            hub.republish_calls();
        }
        return Err(no_session(format!("not connected: {connection:?}")));
    }

    let (reply, answer) = tokio::sync::oneshot::channel();
    if commands
        .send(SessionCommand { action, reply })
        .await
        .is_err()
    {
        // The bridge is gone: the daemon is on its way down.
        return Err(no_session("the session is shutting down"));
    }

    match answer.await {
        Ok(CommandOutcome::Accepted) => Ok(()),
        Ok(CommandOutcome::NoSession(detail)) => Err(no_session(detail)),
        Ok(CommandOutcome::Refused(detail)) => Err(ProtocolError::Refused { detail }),
        // The bridge took the command and died before answering.
        Err(_) => Err(no_session("the session stopped before it answered")),
    }
}

/// The frame that answers a command, whichever way it went.
///
/// One place, because with an id on every answer there is nothing left to
/// special-case: a refusal is an error naming its request, exactly like a
/// refused download or a malformed frame.
fn answer(id: Option<RequestId>, result: Result<(), ProtocolError>) -> Option<String> {
    match result {
        Ok(()) => always(
            id,
            serde_json::to_string(&DaemonMessage::Accepted { id }).map_err(anyhow::Error::from),
        ),
        Err(error) => always(id, error_frame(id, error)),
    }
}

/// The store's footprint: the database plus the journal files SQLite would
/// replay into it. All three are the same data, so all three are counted.
///
/// # Zero on a page, deliberately
///
/// A browser's database is in a VFS rather than on a filesystem, so every
/// `metadata` here fails and the sum is 0 — which Settings shows as `0 B`.
/// Wrong, and it is the least bad of the three answers available. The size is
/// `page_count * page_size`, which needs a query, and this handler is
/// synchronous by the shape of the protocol; the VFS's own `export_db` would
/// answer by copying the whole database into memory, which is precisely what
/// everything else on this side goes out of its way not to do. Fixing it
/// properly means an async usage query through `session/store/`, and that is
/// a wider change than a number in a settings pane is worth today. Recorded
/// in `AGENTS.md` under what is left.
fn database_bytes() -> u64 {
    let base = oxidezap_session::resolve_database_path();
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|suffix| std::fs::metadata(format!("{base}{suffix}")).ok())
        .map(|meta| meta.len())
        .sum()
}

fn no_session(detail: impl Into<String>) -> ProtocolError {
    ProtocolError::NoSession {
        detail: detail.into(),
    }
}

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> Result<()> {
    writer.write_all(line.as_bytes()).await?;
    // Newline-delimited framing: the reader above splits on it, so a frame
    // containing one would desynchronize the stream. serde_json never emits a
    // bare newline inside a value, which the protocol tests pin.
    writer.write_all(b"\n").await?;
    Ok(())
}

// Native only, and not for want of trying. These drive `serve_client` over a
// `tokio::io::duplex` and take the startup lock, so they need `tokio::spawn`
// — which wants a `Send` future the wasm bridge's state deliberately is not —
// and a socket path a page has no filesystem for. The web half of this crate
// has tests of its own that run in a browser; see `plugins/web/tests.rs`.
#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    /// Every request gets exactly one answer, the ones that fail included.
    /// A frame that could not be encoded used to be no frame at all, and the
    /// view that asked waited on it forever with nothing logged.
    #[test]
    fn an_answer_that_cannot_be_encoded_is_still_an_answer() {
        let frame = always(
            Some(oxidezap_ipc::RequestId::from(7u64)),
            Err(anyhow::anyhow!("the encoder gave up")),
        )
        .expect("there is always a frame");
        let parsed: serde_json::Value = serde_json::from_str(&frame).expect("valid json");
        assert_eq!(parsed["type"], "error");
        assert_eq!(parsed["error"]["error"], "malformed");
        assert_eq!(parsed["id"], 7);
    }

    use super::*;

    fn hello(protocol: u32, session_events: bool) -> String {
        serde_json::to_string(&ClientRequest::Hello {
            protocol,
            session_events,
            has_window: true,
        })
        .unwrap()
    }

    #[test]
    fn a_matching_hello_is_accepted() {
        assert_eq!(
            check_hello(&hello(PROTOCOL_VERSION, false)),
            Ok(Attached {
                session_events: false,
                has_window: true
            })
        );
    }

    /// Whether there is a window to raise is the client's to say, and a
    /// client that says nothing is one: every client today is a front end,
    /// and a build predating the field is likelier than a headless tool. See
    /// [`ClientRequest::Hello`].
    #[test]
    fn a_client_is_a_window_unless_it_says_otherwise() {
        let silent = format!(r#"{{"request":"hello","protocol":{PROTOCOL_VERSION}}}"#);
        assert_eq!(
            check_hello(&silent),
            Ok(Attached {
                session_events: false,
                has_window: true
            })
        );

        let watcher =
            format!(r#"{{"request":"hello","protocol":{PROTOCOL_VERSION},"has_window":false}}"#);
        assert_eq!(
            check_hello(&watcher),
            Ok(Attached {
                session_events: false,
                has_window: false
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
                session_events: true,
                has_window: true
            })
        );
        // An older client that does not know the field at all still connects,
        // and gets summaries.
        let line = format!(r#"{{"request":"hello","protocol":{PROTOCOL_VERSION}}}"#);
        assert_eq!(
            check_hello(&line),
            Ok(Attached {
                session_events: false,
                has_window: true
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
            DaemonMessage::Error {
                error: ProtocolError::VersionMismatch { client, daemon },
                ..
            } => {
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
            DaemonMessage::Error {
                error: ProtocolError::Malformed { .. },
                ..
            }
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

    fn bare(request: ClientRequest) -> Request {
        Request::bare(request)
    }

    /// A connection's own answer channel. Tests that do not read it only
    /// need it to exist.
    fn outbox() -> Outbox {
        tokio::sync::mpsc::channel(OUTBOX_CAPACITY).0
    }

    /// A host with nothing loaded, for the requests that are not about
    /// plugins — which is every request but one.
    fn no_plugins() -> Arc<oxidezap_plugin_host::Plugins> {
        Arc::new(oxidezap_plugin_host::Plugins::none(Arc::new(|_| {})))
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

        let request = bare(ClientRequest::SendText {
            jid: "a@s.whatsapp.net".into(),
            text: "hi".into(),
            local_id: None,
            quoted: None,
        });
        let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
        assert!(matches!(
            parse(answer.frame),
            DaemonMessage::Accepted { .. }
        ));
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

        let request = bare(ClientRequest::MarkRead {
            jid: "a@s.whatsapp.net".into(),
            through_message_id: None,
        });
        let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
        assert!(matches!(
            parse(answer.frame),
            DaemonMessage::Error {
                error: ProtocolError::Refused { ref detail },
                ..
            } if detail == "has moved on"
        ));
    }

    /// The other way a command can come back: the account went away while the
    /// request was in the bridge's hands. A different answer, because a client
    /// can see that state coming and wait it out rather than change anything.
    #[tokio::test]
    async fn a_session_lost_mid_command_is_reported_as_such() {
        let hub = connected_hub();
        let (commands, _taken) = bridge(CommandOutcome::NoSession("not connected".into()));

        let request = bare(ClientRequest::SendText {
            jid: "a@s.whatsapp.net".into(),
            text: "hi".into(),
            local_id: None,
            quoted: None,
        });
        let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
        assert!(matches!(
            parse(answer.frame),
            DaemonMessage::Error {
                error: ProtocolError::NoSession { .. },
                ..
            }
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

        let request = bare(ClientRequest::SendText {
            jid: "a@s.whatsapp.net".into(),
            text: "hi".into(),
            local_id: None,
            quoted: None,
        });
        let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
        assert!(matches!(
            parse(answer.frame),
            DaemonMessage::Error {
                error: ProtocolError::NoSession { .. },
                ..
            }
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
            bare(ClientRequest::Shutdown),
            &hub,
            &no_plugins(),
            &commands,
            &outbox(),
        )
        .await;
        assert!(matches!(
            parse(answer.frame),
            DaemonMessage::Accepted { .. }
        ));
        assert!(answer.shutdown, "and only then is the daemon asked to stop");
    }

    /// A frame that does not parse is the client's bug, not a reason to drop
    /// its connection: it gets told, and its next request still works. Driven
    /// through the connection, because parsing is what the connection does —
    /// `handle_request` is handed a request that already parsed.
    #[tokio::test]
    async fn a_malformed_frame_is_answered_and_does_not_end_the_connection() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let hub = connected_hub();
        let (commands, _taken) = bridge(CommandOutcome::Accepted);

        let served = tokio::spawn(serve_client(
            server,
            Arc::clone(&hub),
            no_plugins(),
            commands,
        ));
        client
            .write_all(format!("{}\n", hello(PROTOCOL_VERSION, false)).as_bytes())
            .await
            .unwrap();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap(); // the snapshot

        reader
            .get_mut()
            .write_all(b"not json at all\n")
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(
            matches!(
                serde_json::from_str::<DaemonMessage>(&line).unwrap(),
                DaemonMessage::Error {
                    error: ProtocolError::Malformed { .. },
                    ..
                }
            ),
            "expected a complaint, got {line}"
        );

        // Still usable: the next request is answered rather than the
        // connection being gone.
        let snapshot = serde_json::to_string(&Request::bare(ClientRequest::Snapshot)).unwrap();
        reader
            .get_mut()
            .write_all(format!("{snapshot}\n").as_bytes())
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonMessage>(&line).unwrap(),
            DaemonMessage::Hello { .. }
        ));
        served.abort();
    }

    /// The daemon owns no window, so this is a relay: whoever has one is the
    /// only party that can raise it.
    #[tokio::test]
    async fn a_window_request_is_published_rather_than_acted_on() {
        let hub = StateHub::new();
        let (commands, taken) = bridge(CommandOutcome::Accepted);
        let mut signals = hub.subscribe_signals();

        let request = bare(ClientRequest::ShowWindow);
        let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
        assert!(matches!(
            parse(answer.frame),
            DaemonMessage::Accepted { .. }
        ));

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
        let frames = (oxidezap_ipc::MAX_REQUEST_BYTES / 1000) + 10;
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            for _ in 0..frames {
                let _ = client.write_all(frame.as_bytes()).await;
                let _ = client.write_all(b"\n").await;
            }
        });

        for i in 0..frames {
            match read_frame(&mut reader, &mut buf).await {
                Ok(Some(oxidezap_ipc::FrameRead::Line(line))) => assert_eq!(line.len(), 1000),
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
            read_frame(&mut reader, &mut buf).await,
            Ok(Some(oxidezap_ipc::FrameRead::NotUtf8))
        ));
        // The stream survives it.
        assert!(matches!(
            read_frame(&mut reader, &mut buf).await,
            Ok(Some(oxidezap_ipc::FrameRead::Line(_)))
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
            frame = read_frame(&mut reader, &mut buf) => {
                panic!("an unterminated frame must not complete: {frame:?}");
            }
            () = std::future::ready(()) => {}
        }
        assert!(!buf.is_empty(), "the prefix was consumed and kept");

        client.write_all(b"shot\"}\n").await.unwrap();
        match read_frame(&mut reader, &mut buf).await {
            Ok(Some(oxidezap_ipc::FrameRead::Line(line))) => {
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
        let mut buf = vec![b'x'; oxidezap_ipc::MAX_REQUEST_BYTES];

        assert!(matches!(
            read_frame(&mut reader, &mut buf).await,
            Ok(Some(oxidezap_ipc::FrameRead::TooLong))
        ));
        assert!(buf.is_empty(), "a refused frame leaves nothing behind");
        drop(client);
    }

    /// An encoding bug in the opening frame is as recoverable as one after it.
    /// Closing on it silently leaves the client unable to tell a rejected
    /// hello from a dead socket.
    #[tokio::test]
    async fn a_hello_that_is_not_text_is_answered_rather_than_dropped() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (reader, mut writer) = tokio::io::split(server);
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
            DaemonMessage::Error {
                error: ProtocolError::Malformed { .. },
                ..
            }
        ));
    }

    /// The regression this replaced: a summary-only client was handed its
    /// snapshot and then dropped, because the branch serving session events
    /// ended the connection on a closed channel and opting out produced one.
    #[tokio::test]
    async fn a_client_that_wants_only_summaries_stays_connected() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let hub = connected_hub();
        let (commands, _taken) = bridge(CommandOutcome::Accepted);

        let served = tokio::spawn(serve_client(
            server,
            Arc::clone(&hub),
            no_plugins(),
            commands,
        ));
        client
            .write_all(format!("{}\n", hello(PROTOCOL_VERSION, false)).as_bytes())
            .await
            .unwrap();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(
            matches!(
                serde_json::from_str::<DaemonMessage>(&line).unwrap(),
                DaemonMessage::Hello { .. }
            ),
            "expected a snapshot, got {line}"
        );

        // Still there: a summary reaches it rather than an EOF.
        hub.apply(crate::state::Change::live(
            oxidezap_ipc::DaemonEvent::ChatRemoved {
                jid: "a@s.whatsapp.net".into(),
            },
        ));
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(
            matches!(
                serde_json::from_str::<DaemonMessage>(&line).unwrap(),
                DaemonMessage::Update { .. }
            ),
            "the connection was dropped instead: {line}"
        );
        served.abort();
    }

    /// The other half: a client that asked for events gets them, and the
    /// summary stream keeps working alongside.
    #[tokio::test]
    async fn a_client_that_asked_for_events_receives_them() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let hub = connected_hub();
        let (commands, _taken) = bridge(CommandOutcome::Accepted);

        let served = tokio::spawn(serve_client(
            server,
            Arc::clone(&hub),
            no_plugins(),
            commands,
        ));
        client
            .write_all(format!("{}\n", hello(PROTOCOL_VERSION, true)).as_bytes())
            .await
            .unwrap();

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap(); // the hello

        hub.publish_session(
            serde_json::to_string(&DaemonMessage::Session {
                event: Box::new(oxidezap_core::UiEvent::Connected),
            })
            .unwrap(),
        );
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        assert!(
            matches!(
                serde_json::from_str::<DaemonMessage>(&line).unwrap(),
                DaemonMessage::Session { .. }
            ),
            "expected a session event, got {line}"
        );
        served.abort();
    }

    /// Forgetting the session is the only way out of dead credentials, and
    /// dead credentials are a state the account is unreachable in. Gating it
    /// on a connection refuses it exactly when it is wanted.
    #[test]
    fn the_local_actions_do_not_need_a_connection() {
        assert!(!Action::ForgetSession.needs_network());
        assert!(!Action::ReloadHistory.needs_network());
        // A view is one local row and no stanza, over history a disconnected
        // window can still read — and the ring it watched is already drawn.
        assert!(
            !Action::MarkStatusWatched {
                message_ids: vec!["3EB0".into()],
            }
            .needs_network()
        );
        assert!(
            Action::SendText {
                jid: "a@s.whatsapp.net".into(),
                text: "hi".into(),
                local_id: None,
                quoted: None,
            }
            .needs_network()
        );
    }

    /// A refused command names the request it refused. Before ids, the only
    /// way to report a refused send was to invent a failure against the
    /// message the client happened to have drawn.
    #[tokio::test]
    async fn a_refusal_names_the_request_it_refused() {
        // Not connected, so the send is refused at the door.
        let hub = StateHub::new();
        let (commands, _taken) = bridge(CommandOutcome::Accepted);

        let request = Request {
            id: Some(42),
            request: ClientRequest::SendText {
                jid: "a@s.whatsapp.net".into(),
                text: "hi".into(),
                local_id: Some("local_1".into()),
                quoted: None,
            },
        };
        let answer = handle_request(request, &hub, &no_plugins(), &commands, &outbox()).await;
        assert!(matches!(
            parse(answer.frame),
            DaemonMessage::Error {
                id: Some(42),
                error: ProtocolError::NoSession { .. },
            }
        ));
    }

    /// A peer that connects and says nothing costs a task and a descriptor
    /// for as long as it likes. A reconnect loop doing it takes the listener
    /// down, and the daemon treats a dead listener as fatal.
    #[tokio::test(start_paused = true)]
    async fn a_client_that_never_speaks_does_not_hold_its_slot_forever() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let hub = StateHub::new();
        let (commands, _taken) = bridge(CommandOutcome::Accepted);

        // Returns rather than parking forever; the paused clock reaches the
        // handshake deadline as soon as nothing else can run.
        serve_client(server, hub, no_plugins(), commands)
            .await
            .unwrap();

        let mut answer = String::new();
        BufReader::new(client).read_line(&mut answer).await.unwrap();
        assert!(
            matches!(
                serde_json::from_str::<DaemonMessage>(&answer).unwrap(),
                DaemonMessage::Error {
                    error: ProtocolError::Malformed { .. },
                    ..
                }
            ),
            "and it is told why: {answer}"
        );
    }

    /// A client turned away has to be able to tell "full" from "broken", or
    /// it retries against a daemon that will keep refusing it.
    #[tokio::test]
    async fn a_refused_client_is_told_the_daemon_is_full() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        reject(server).await;

        let mut answer = String::new();
        BufReader::new(client).read_line(&mut answer).await.unwrap();
        assert!(matches!(
            serde_json::from_str::<DaemonMessage>(&answer).unwrap(),
            DaemonMessage::Error {
                error: ProtocolError::TooManyClients { limit },
                ..
            } if limit == MAX_CLIENTS
        ));
    }

    /// Two daemons starting together can both see a stale socket; the lock is
    /// what stops the second from unlinking the first's freshly bound one.
    ///
    /// # Why the release is waited for rather than asserted outright
    ///
    /// `flock` is released when the *last* descriptor on the open file
    /// description closes, and a `fork` anywhere in this process duplicates
    /// every one of them: between the fork and the exec that clears them,
    /// a child holds a copy of this lock and closing ours releases nothing.
    /// Measured outside the suite at ~5% of attempts against a single
    /// spawning thread, and it is what failed this test on macOS while Linux
    /// got away with it.
    ///
    /// [`crate::one_at_a_time`] keeps this away from the tests that spawn,
    /// which is worth doing on its own — but it cannot cover a fork this
    /// crate does not make, and a test that fails when some library forks
    /// beside it is testing the wrong thing. The property is that the lock
    /// does not *outlive its holder*: a copy in a child that is microseconds
    /// from exec is not the holder, so the wait is what separates the two.
    /// A lock genuinely never released still fails, which is the bug this
    /// test exists for.
    #[test]
    fn the_startup_lock_is_exclusive() {
        let _exclusive = crate::one_at_a_time();
        let dir = std::env::temp_dir().join(format!("oxidezap-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("daemon.sock");

        let first = acquire_startup_lock(&socket).expect("first daemon takes the lock");
        assert!(
            acquire_startup_lock(&socket).is_err(),
            "a second daemon must not get in"
        );

        // Released with the handle, so a restart is not blocked by the last
        // run.
        //
        // Retried, and not because the property is doubtful: on an idle
        // machine the first attempt succeeds and this loop never sleeps. It
        // is here because the immediate assertion failed twice on the macOS
        // runner and nowhere else, which a single attempt reports as "the
        // lock outlived its holder" — a claim about this code that the
        // evidence does not support, since re-acquiring works everywhere it
        // can be reproduced.
        //
        // The likeliest mechanism is that a `flock` belongs to the *open file
        // description*, which `fork` duplicates: a child spawned by another
        // test in this binary (`window::tests::launching` starts a shell that
        // sleeps) holds this descriptor from the moment it is forked until it
        // execs, so dropping the handle here releases nothing until it does.
        // That is a hypothesis — it did not reproduce under load here — which
        // is why this waits for the lock rather than asserting anything about
        // why it was briefly unavailable. What it still refuses is a lock
        // that is never released.
        drop(first);
        // The library's clock, which is what this repo uses everywhere: a
        // test that moved time would move this with it.
        let deadline = wacore::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut last = None;
        let regained = loop {
            match acquire_startup_lock(&socket) {
                Ok(lock) => break Some(lock),
                Err(e) if wacore::time::Instant::now() < deadline => {
                    last = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => {
                    last = Some(e);
                    break None;
                }
            }
        };
        assert!(
            regained.is_some(),
            "lock outlived its holder: {}",
            last.map_or_else(|| "no reason given".to_string(), |e| e.to_string())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The fallback directory sits at a predictable path in a world-writable
    /// place, so a symlink planted there must not be followed.
    ///
    /// Unix only, and not for want of porting: on Windows the state directory
    /// is under the user's own profile, so there is no world-writable parent
    /// for anyone to plant anything in, and `prepare_state_dir` has nothing
    /// to check.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_socket_dir_is_refused() {
        let base = std::env::temp_dir().join(format!("oxidezap-symlink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let target = base.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        let link = base.join("sockdir");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = prepare_state_dir(&link).expect_err("a symlink must be refused");
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected reason: {err}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A directory we already own is reused, and tightened if it is loose.
    ///
    /// Unix only, for the same reason as the symlink check above.
    #[cfg(unix)]
    #[test]
    fn a_loose_but_owned_dir_is_tightened_rather_than_refused() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("oxidezap-loose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();

        prepare_state_dir(&dir).expect("our own directory is usable");

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "left readable by other users");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
