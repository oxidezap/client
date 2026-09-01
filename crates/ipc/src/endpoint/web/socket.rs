//! The WebSocket itself: one task owns it, and callers get a channel.
//!
//! The transport half of this directory, and the only half of it /AGENTS.md
//! is about. What it carries is the same newline-delimited JSON the socket
//! and the pipe carry, one frame per message, and the framing above it is
//! written once elsewhere.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender, channel};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

use super::address::without_secrets;
use crate::Link;

/// How many frames may wait for a socket that has not opened yet.
///
/// There is normally one — the hello, written before the connection is up.
/// The rest is slack for a front end that asks for something immediately.
const MAX_HELD_FRAMES: usize = 64;

/// How many frames off the socket may wait to be applied.
///
/// A browser hands a page everything that arrives and offers no way to say
/// "not yet", so the only back pressure available is this queue refusing to
/// grow. It has to refuse: applying a frame means fetching the media it names
/// first, which carries a budget measured in tens of seconds, while a daemon
/// with a large history writes its hydration frames as fast as the socket
/// takes them. Unbounded, that is every remaining frame's JSON held at once
/// against a linear memory with a one-gigabyte ceiling — and an allocation
/// that fails there is an abort, not an error.
///
/// Deep enough that an ordinary burst never reaches it, and a connection that
/// does reach it ends with a reason rather than losing frames one at a time:
/// the front end is tracking requests against them, and `Frames::finish`
/// fails all of those at once so the views that asked can ask again.
///
/// What it does *not* promise is that no frame is lost, and the difference is
/// worth stating rather than discovering. Everything queued is delivered, and
/// so is the frame that hit the bound — but frames the browser had already
/// dispatched behind it are dropped, because the alternative to dropping them
/// is going on accepting from a producer this page has just proven it cannot
/// match, which is the unbounded queue again. State survives that: it carries
/// a version and the reconnect's snapshot brings it back. News does not, so a
/// `SendFailed` landing in the window between the bound being hit and the
/// socket closing is genuinely gone. Closing it is not on this side of the
/// wire — a page cannot exert back pressure over a WebSocket — and belongs
/// with the daemon coalescing pending state frames rather than with a second
/// bounded queue here, which would only ask the same question one level down.
const MAX_QUEUED_FRAMES: usize = 256;

/// What the socket says, in the order it says it.
///
/// One channel rather than one per event, because the order is the point: a
/// frame that arrived before the close has to be handled before the close is.
#[derive(Debug)]
pub enum FromSocket {
    /// The socket is up. Nothing was sent before this; anything a caller
    /// wrote first was held and flushed here.
    Open,
    /// One frame, exactly as it came off the wire.
    Line(String),
    /// The connection ended, and why as far as the browser will say.
    Closed(String),
}

/// What the socket says, in the order it says it, plus the one fact that
/// travels beside the queue rather than in it.
///
/// Nothing jumps the queue. Every ending — an ordinary close and an
/// overflow alike — is delivered behind the frames that arrived before it,
/// because a frame is not always recoverable: state carries a version and
/// comes back in a snapshot, but a `SendFailed` or a window request is news,
/// and a reconnect brings back no copy of it. A backlog abandoned to reach
/// the ending sooner is that news thrown away.
///
/// What does travel out of band is [`connection_ended`](Self::connection_ended),
/// which is a different claim: not *what* happened but *that* the socket has
/// already gone, true while the reader is still working through the backlog.
/// That is what makes delivering the ending in order affordable — see the
/// method.
pub struct Inbound {
    queued: Receiver<FromSocket>,
    ended: Rc<Cell<bool>>,
}

impl Inbound {
    /// Whether the socket has already gone, ahead of the queue saying so.
    ///
    /// The frames still queued are worth applying, for the reason above. What
    /// is not worth waiting for is the *media* they name: it is fetched from
    /// a bridge that has just closed, and a budget spent per frame is what
    /// would put hours between the socket going and the front end's pending
    /// requests being failed. A reader that asks this drains in order and
    /// skips the optional sideband, which the renderer already draws as an
    /// offer to download — but keeps fetching a `Downloaded`, which is
    /// somebody's answer rather than a frame's decoration.
    #[must_use]
    pub fn connection_ended(&self) -> bool {
        self.ended.get()
    }

    /// The next thing that happened, oldest first.
    ///
    /// `None` once the sender is gone and the queue is empty.
    pub async fn recv(&mut self) -> Option<FromSocket> {
        self.queued.recv().await
    }
}

/// Connect to a daemon over a WebSocket.
///
/// Returns immediately: the socket is still opening, and anything written in
/// the meantime waits for it rather than failing. A connection that never
/// opens arrives as [`FromSocket::Closed`], which is the same shape as one
/// that opened and went away — a caller has one path to recover through
/// either way.
///
/// # Errors
///
/// Only for a URL the browser refuses outright — a bad scheme, or a mixed
/// content block. Everything that fails later is reported on the channel.
pub fn connect(url: &str) -> Result<(Link, Inbound), String> {
    let socket = WebSocket::new(url).map_err(|e| {
        // Redacted, like the two log paths. This one is worse than a log: it
        // is the string the window *draws*, so the token would be in any
        // screenshot of the failure as well as in anything copied out of it.
        format!(
            "could not open a socket to {}: {}",
            without_secrets(url),
            e.as_string()
                .unwrap_or_else(|| "refused by the browser".into())
        )
    })?;

    let (inbound, queued) = channel::<FromSocket>(MAX_QUEUED_FRAMES);
    let (outbound, mut to_send) = tokio::sync::mpsc::unbounded_channel::<String>();
    // Out of band: true from the moment the socket is done, while the frames
    // the reader still has to apply are queued behind it. See
    // [`Inbound::connection_ended`].
    let ended = Rc::new(Cell::new(false));
    // Whether somebody has already taken responsibility for putting the
    // ending on the queue. Exactly one sender may, because a queue that is
    // full is one where a second `Closed` waits on the same freed slot as the
    // first — and the reader stops at whichever arrives, so a race there is a
    // frame silently never applied. Closing the socket ourselves is what
    // makes this two callbacks rather than one: `close()` brings `onclose`
    // along behind it.
    let ending = Rc::new(Cell::new(false));

    // Shared with the `onopen` callback so a frame written before the socket
    // was up is sent rather than dropped.
    let is_open = Rc::new(Cell::new(false));
    let held = Rc::new(RefCell::new(Vec::<String>::new()));
    // Closed by the `onclose` callback. The writer races its receive against
    // this, because a socket that closes while the writer is parked would
    // otherwise leave it parked: the next `send_line` would queue happily,
    // the caller would record a request against it, and nothing would ever
    // fail that request or answer it.
    let (gone, closed) = futures_channel::oneshot::channel::<()>();

    // Held, not forgotten. `Closure::forget` hands a callback to the JS heap
    // for the life of the page, and the front end reconnects — so every
    // dropped connection would leave four more behind. These live exactly as
    // long as the task below, which is exactly as long as the socket.
    // Each is bound to what its own block produces rather than declared
    // empty and filled in: a binding that is written before it is ever read
    // is one the compiler is right to call out, and there is no second
    // assignment for these to be waiting on.
    let mut callbacks: Vec<Closure<dyn FnMut()>> = Vec::new();

    {
        let held_socket = socket.clone();
        let is_open = Rc::clone(&is_open);
        let held = Rc::clone(&held);
        let inbound = inbound.clone();
        let opened = Closure::<dyn FnMut()>::new(move || {
            is_open.set(true);
            for frame in held.borrow_mut().drain(..) {
                if let Err(e) = held_socket.send_with_str(&frame) {
                    log::error!("could not send a held frame: {e:?}");
                }
            }
            deliver(&inbound, FromSocket::Open);
        });
        socket.set_onopen(Some(opened.as_ref().unchecked_ref()));
        callbacks.push(opened);
    }

    let message_callback = {
        let inbound = inbound.clone();
        let ended = Rc::clone(&ended);
        let ending = Rc::clone(&ending);
        let socket_here = socket.clone();
        let message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            // Whatever else arrives on a socket already being torn down is
            // past the bound that ended it, so it is not queued at all.
            if ended.get() {
                return;
            }
            // Text only. The daemon speaks JSON and a binary frame would be
            // something else entirely — saying so beats decoding whatever it
            // is and failing to parse it a layer up.
            let Some(line) = event.data().as_string() else {
                log::warn!("ignoring a non-text frame from the daemon");
                return;
            };
            // `try_send`, because this is a browser callback and there is
            // nothing here to await on. A full queue is the one case that
            // matters: see [`MAX_QUEUED_FRAMES`].
            if let Err(TrySendError::Full(FromSocket::Line(line))) =
                inbound.try_send(FromSocket::Line(line))
            {
                log::error!(
                    "the daemon is sending frames faster than this page can apply them; \
                     ending the connection"
                );
                // Set first, so the frames already queued drain without
                // paying for their media — which is what makes announcing
                // the ending *behind* them affordable rather than a wait
                // measured in hours.
                ended.set(true);
                // The frame that would not fit is kept, not dropped. It is
                // one frame — the bound is already closed above it, since
                // nothing more is queued once `ended` is set — and it may be
                // news, which no snapshot brings back. It goes ahead of the
                // ending, in the order it arrived, which is why both are sent
                // from *one* task rather than two: two would race, and the
                // ending winning that race is this frame lost after all.
                //
                // Claimed here, so the `onclose` that `close()` is about to
                // fire leaves the ending to this task rather than queueing a
                // second one to race it.
                ending.set(true);
                let inbound = inbound.clone();
                spawn_local(async move {
                    if inbound.send(FromSocket::Line(line)).await.is_ok() {
                        let _ = inbound
                            .send(FromSocket::Closed(
                                "the daemon sent frames faster than this page could apply them"
                                    .to_string(),
                            ))
                            .await;
                    }
                });
                let _ = socket_here.close();
            }
        });
        socket.set_onmessage(Some(message.as_ref().unchecked_ref()));
        message
    };

    let close_callback = {
        let inbound = inbound.clone();
        let ended = Rc::clone(&ended);
        let ending = Rc::clone(&ending);
        let mut gone = Some(gone);
        let closed = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
            // Before the frame is queued, because the point of it is to be
            // true while the reader is still working through the backlog this
            // close is behind. See [`Inbound::connection_ended`].
            ended.set(true);
            // Unless the overflow above already claimed it, in which case its
            // task is carrying the retained frame and the ending together and
            // a second `Closed` here would only race that frame to the queue's
            // next free slot.
            if !ending.replace(true) {
                let reason = event.reason();
                let detail = if reason.is_empty() {
                    format!("the daemon connection closed (code {})", event.code())
                } else {
                    format!("the daemon connection closed: {reason}")
                };
                deliver(&inbound, FromSocket::Closed(detail));
            }
            // Wakes the writer, which is what ends it. Without this it stays
            // parked on its receive: the next `send_line` would queue
            // happily, the caller would record a request against it, and
            // nothing would ever answer or fail that request.
            drop(gone.take());
        });
        socket.set_onclose(Some(closed.as_ref().unchecked_ref()));
        closed
    };

    let error_callback = {
        // `onerror` carries nothing useful in a browser — the event is
        // deliberately opaque, so a page cannot probe the network with it —
        // and `onclose` always follows. Logging it is all there is to do.
        let failed = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            log::warn!("the daemon socket reported an error; waiting for the close");
        });
        socket.set_onerror(Some(failed.as_ref().unchecked_ref()));
        failed
    };

    // The one place the socket is written from. Everything else queues.
    spawn_local(async move {
        let mut closed = closed;
        loop {
            let next = futures_lite::future::or(async { to_send.recv().await }, async {
                // Resolves when the socket closed; the sender is only
                // ever dropped, never sent on.
                let _ = (&mut closed).await;
                None
            })
            .await;
            let Some(frame) = next else { break };
            if is_open.get() {
                if let Err(e) = socket.send_with_str(&frame) {
                    log::error!("could not reach the daemon: {e:?}");
                    break;
                }
            } else if held.borrow().len() < MAX_HELD_FRAMES {
                held.borrow_mut().push(frame);
            } else {
                // A daemon that accepts the connection and never completes
                // the handshake would otherwise have this grow for as long as
                // the front end kept asking. The connection is over, and it
                // ends here rather than losing frames one at a time: the
                // caller was already told the frame was sent, and a request
                // it is tracking would then wait for an answer nothing can
                // produce. Ending it is what runs `Frames::finish`, which
                // fails every one of them at once and lets the views that
                // asked ask again.
                deliver(
                    &inbound,
                    FromSocket::Closed(
                        "the daemon accepted the connection but never opened it".to_string(),
                    ),
                );
                break;
            }
        }
        // The sender was dropped, or a write failed: either way this
        // connection is over, and a socket left open would keep the daemon
        // holding a client slot for it.
        //
        // The handlers come off before the closures are dropped: a browser
        // holding a reference to a freed callback is a crash rather than a
        // missed event.
        socket.set_onopen(None);
        socket.set_onmessage(None);
        socket.set_onclose(None);
        socket.set_onerror(None);
        let _ = socket.close();
        drop(callbacks);
        drop(message_callback);
        drop(close_callback);
        drop(error_callback);
    });

    Ok((Link::over_socket(outbound), Inbound { queued, ended }))
}

/// Put one event on the queue, waiting for room only where waiting is right.
///
/// `Open` and `Closed` are one apiece and say what happened to the
/// connection, so losing one to a full queue would leave the reader waiting
/// on a socket that has already gone. A `Line` is not sent through here: a
/// task per overflowing frame would be the unbounded queue again, wearing the
/// scheduler's clothes. The single frame that *triggers* an overflow is the
/// exception, and is not an exception to the bound: it is spent on the one
/// frame that closes the connection, after which nothing else is queued at
/// all.
fn deliver(inbound: &Sender<FromSocket>, event: FromSocket) {
    match inbound.try_send(event) {
        Ok(()) | Err(TrySendError::Closed(_)) => {}
        Err(TrySendError::Full(event)) => {
            let inbound = inbound.clone();
            spawn_local(async move {
                let _ = inbound.send(event).await;
            });
        }
    }
}
