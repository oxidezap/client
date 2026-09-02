//! The local socket front ends connect to.
//!
//! One task per connection, each owning its own writer. Nothing here mutates
//! daemon state directly: requests go to the session, changes come back
//! through [`StateHub`], which is what keeps two clients from racing each
//! other into an inconsistent view.
//!
//! # What is here and what is next door
//!
//! This file is the part every platform has: the caps a connection is
//! admitted under, the loop that serves one, and the frames every answer is
//! built out of. Beside it,
//!
//! * `handshake` reads frames and decides whether a peer may proceed,
//! * `requests` turns an accepted frame into an action and an answer,
//! * `accept` is the half that needs an operating system — a directory to
//!   claim, a lock to hold, a listener to accept on — and is the only code
//!   here a page does not compile.
//!
//! A page reaches `serve_client` with a duplex rather than a socket, so it
//! compiles everything except that last file and nothing here has to know
//! which of the two it is serving.

// The whole of the platform gate, and there is no web half to pair it with: a
// page has no listener to accept on, and everything below is what it runs
// instead. Re-exported rather than made a public module because the binary
// asks for `server::claim` and `server::run`, which is where they read best.
#[cfg(not(target_family = "wasm"))]
mod accept;
#[cfg(not(target_family = "wasm"))]
pub use accept::{Claim, claim, run};
// Not `pub`: the only caller is the web bridge, one directory over.
#[cfg(not(target_family = "wasm"))]
pub(crate) use accept::too_many_clients_frame;

mod handshake;
mod requests;

// Gated the same way, and the only other thing in the module that is: they
// drive a connection end to end, which needs `tokio::spawn` and a filesystem.
// Their own header says why a page cannot run them.
#[cfg(all(test, not(target_family = "wasm")))]
mod tests;

use std::sync::Arc;

use anyhow::{Context, Result};
use oxidezap_ipc::{ClientRequest, DaemonMessage, ProtocolError, Request, RequestId};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError;

use crate::session_bridge::{Action, Commands};
use crate::state::StateHub;

use handshake::{handshake, read_frame};
use requests::{dispatch, handle_request};

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
                    // Debug, not trace: this is the one event that blanks both
                    // panes. The client answers it by dropping every reference
                    // it holds and waiting for a keyframe -- on BOTH streams,
                    // since the channel that lagged carries both -- so a call
                    // whose video "does not work" looks from every other log
                    // line like a call whose video is fine. At trace it was
                    // absent from the level a page is actually left at.
                    log::debug!("client missed {missed} video frames");
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

async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, line: &str) -> Result<()> {
    writer.write_all(line.as_bytes()).await?;
    // Newline-delimited framing: the reader above splits on it, so a frame
    // containing one would desynchronize the stream. serde_json never emits a
    // bare newline inside a value, which the protocol tests pin.
    writer.write_all(b"\n").await?;
    Ok(())
}
