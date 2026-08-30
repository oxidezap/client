//! The client end of the daemon connection, when the client is a page.
//!
//! A browser tab has no filesystem to find a socket in and no thread to park
//! in a read, so the third transport is a WebSocket: the same
//! newline-delimited JSON, one frame per message, with the newline dropped
//! because the socket already frames.
//!
//! Written against `web-sys` rather than through hand-written glue: the
//! bindings this needs — `WebSocket`, `MessageEvent`, `CloseEvent`,
//! `Location`, `fetch` — all exist in Rust already, so nothing here is
//! JavaScript.
//!
//! # Why the socket is never handed out
//!
//! A `web_sys::WebSocket` is a JS object: neither `Send` nor `Sync`, and only
//! usable from the thread that made it. A front end holds its connection
//! beside the rest of its state and writes to it from wherever a click lands,
//! which a JS object cannot support. So the socket stays inside one
//! `spawn_local` task and callers get a channel into it — see
//! [`crate::Link`]. That also gives sends before the socket opens somewhere to
//! wait, which matters because the very first frame a front end writes is its
//! hello.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender, channel};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::spawn_local;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

use crate::Link;

/// How long one media payload may take before it is treated as unavailable.
///
/// Generous: the bridge is normally on the same machine, and this exists to
/// bound a hang rather than to police a slow link.
const MEDIA_TIMEOUT_MS: i32 = 30_000;

/// How long staging a payload may take.
///
/// Longer than a read, because this one has no second chance: media the page
/// failed to fetch is drawn as an offer to download again, and a recording
/// that fails to stage is a message the person already watched themselves
/// send.
const UPLOAD_TIMEOUT_MS: i32 = 60_000;

/// A `setTimeout` that aborts a fetch, cleared when the fetch finishes first.
struct FetchDeadline {
    handle: i32,
    _fire: Closure<dyn FnMut()>,
}

impl FetchDeadline {
    fn arm(
        window: &web_sys::Window,
        abort: &web_sys::AbortController,
        millis: i32,
    ) -> Result<Self, String> {
        let abort = abort.clone();
        let fire = Closure::<dyn FnMut()>::new(move || abort.abort());
        let handle = window
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                fire.as_ref().unchecked_ref(),
                millis,
            )
            .map_err(|e| format!("could not arm a fetch timeout: {e:?}"))?;
        Ok(Self {
            handle,
            _fire: fire,
        })
    }
}

impl Drop for FetchDeadline {
    fn drop(&mut self) {
        if let Some(window) = web_sys::window() {
            window.clear_timeout_with_handle(self.handle);
        }
    }
}

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

/// Where to look for a daemon, as the page was asked to.
///
/// `?daemon=<url>` first, because the page is static and the daemon is not:
/// one build is served to everybody and each person's daemon is their own.
/// Failing that, the loopback default — which is where a daemon started by
/// hand on the same machine listens.
#[must_use]
pub fn endpoint_url() -> String {
    let default = || {
        format!(
            "ws://127.0.0.1:{}{}",
            crate::DEFAULT_WEB_PORT,
            crate::WEB_SOCKET_PATH
        )
    };
    match named_daemon() {
        NamedDaemon::Named(asked) => asked,
        // A rejected one falls back here on purpose: this function answers
        // "where would a daemon be", and the caller that must not proceed on a
        // rejection is the one that matches on [`named_daemon`] itself.
        NamedDaemon::Nobody | NamedDaemon::Rejected(_) => default(),
    }
}

/// What this page was told to attach to.
///
/// Three answers, and the third is why this is not an `Option`. "Nobody named
/// one" is a page that runs its own session; "named one we will not use" is a
/// configuration error, and collapsing it into the first silently opens a
/// *different* session — against this origin's own store — for somebody whose
/// only mistake was a typo in a URL, or whose daemon was refused for exactly
/// the reason the check exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedDaemon {
    /// No `daemon` parameter at all.
    Nobody,
    /// One named, and usable.
    Named(String),
    /// One named, and refused. The string is for a person, and carries no
    /// token.
    Rejected(String),
}

/// The daemon this page was pointed at, if it was pointed at one.
#[must_use]
pub fn named_daemon() -> NamedDaemon {
    let asked = match read_parameter("daemon") {
        Parameter::Present(asked) => asked,
        Parameter::Absent => return NamedDaemon::Nobody,
        // Named, and not readable — a truncated `%` in a pasted URL is the
        // usual way. Refused rather than ignored: ignoring it opens a session
        // against this origin's own store for somebody who asked for a
        // daemon, which is the substitution `Rejected` exists to prevent.
        Parameter::Unreadable => {
            log::error!("ignoring #daemon=: the value is not decodable");
            return NamedDaemon::Rejected(
                "The #daemon in this page's address could not be read — a percent escape in it \
                 is incomplete. Correct it, or remove it to let this page run its own session."
                    .to_string(),
            );
        }
    };
    // A query parameter is whatever put the user on this page, which may be a
    // link somebody sent them. The daemon it names is handed the message
    // history and can be told to send, so an unchecked one turns a link into
    // a way to point the window at somebody else's server.
    match usable_endpoint(&asked) {
        Ok(()) => NamedDaemon::Named(asked),
        Err(why) => {
            // Redacted, here and in what is shown. A rejected URL is the
            // *likeliest* one to be pasted into an issue — it is the one that
            // did not work — and it carries the same token the accepted one
            // does.
            let named = without_secrets(&asked);
            log::error!("ignoring #daemon={named}: {why}");
            NamedDaemon::Rejected(format!(
                "This page was pointed at {named}, which it will not use: {why}. \
                 Correct the #daemon in the address, or remove it to let this \
                 page run its own session."
            ))
        }
    }
}

/// # Known gap: the daemon is not authenticated to the page
///
/// The token proves the *page* to the daemon. Nothing proves the daemon to
/// the page, and on a loopback TCP port that asymmetry has teeth: another
/// account on the machine can bind the predictable port first, and a
/// bookmarked URL opened while the real daemon is down hands that process the
/// token in its handshake. It can then release the port, wait, and use the
/// token against the real daemon — `Origin` is a string it also controls.
///
/// The native endpoint has no such gap: a Unix socket has a peer uid, and the
/// client checks who answered. A browser cannot ask that of a TCP port.
///
/// Closing it means mutual authentication, and the shapes trade against each
/// other rather than one being obviously right:
///
/// - **Server-first challenge.** Connect carrying nothing, send a nonce, and
///   let the daemon prove it holds the token before the page offers its own
///   proof. Nothing is ever disclosed to an impostor. The cost is that the
///   upgrade becomes unauthenticated, so the endpoint stops being able to
///   answer `404` to strangers — the concealment described above is spent to
///   buy this.
/// - **Proof in the query.** Send `HMAC(token, nonce)` instead of the token.
///   Keeps the `404`, but a proof replayed with its own nonce is as good as
///   the token unless the daemon remembers nonces, which is state and a
///   clock.
///
/// Both are a wire-protocol change on two ends plus the media path, which
/// authenticates per request. Until one is chosen, `--web` carries this: it
/// is off by default, and the threat is a hostile account on the same
/// machine.

/// A URL fit to be written down.
///
/// The query is where the token lives, so it is what comes off: everything
/// that identifies *which* daemon survives, and the credential that admits
/// you to it does not. Not a parser — a token is only ever in the query, and
/// splitting there cannot accidentally keep one.
#[must_use]
pub fn without_secrets(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Whether a page may attach to this URL without being asked again.
///
/// Two rules. It has to be a WebSocket URL, because anything else is a
/// mistake or an attempt at something. And it has to name either this machine
/// or the origin the page was itself served from — a daemon somewhere else
/// entirely is a decision, not a default, and a link is not how it should be
/// made.
///
/// Parsed with the browser's own URL parser rather than by splitting on
/// characters, because *the browser* is what will resolve it and only its
/// answer is the one that matters. Splitting is how
/// `wss://127.0.0.1:9527@evil.example/ws` gets through a host check: the part
/// before the `@` is userinfo, so a reader looking for a colon finds
/// `127.0.0.1` while the socket opens to `evil.example`.
///
/// # Errors
///
/// The reason, for the log: this is a silent fallback to the loopback default
/// rather than a failure to start, because a page that refuses to load is
/// worse than one that attaches where it was going to anyway.
fn usable_endpoint(url: &str) -> Result<(), String> {
    let parsed = web_sys::Url::new(url).map_err(|_| "not a URL".to_string())?;
    if !matches!(parsed.protocol().as_str(), "ws:" | "wss:") {
        return Err(format!("{} is not a WebSocket scheme", parsed.protocol()));
    }
    // `hostname`, not `host`: no port, no userinfo, and already lowercased
    // and unwrapped from the brackets an IPv6 literal carries.
    let host = parsed.hostname();
    if super::is_loopback_host(&host) {
        return Ok(());
    }
    // The page's own origin: a deployment that serves the bridge beside
    // itself is naming where it already came from.
    //
    // The *whole* origin, not the hostname. A host is not an origin — a
    // different port is a different origin, and on a shared or nonstandard
    // host it is very likely a different owner. Comparing hostnames alone let
    // `#daemon=wss://this-host:8443/ws` pass as "where this page came from",
    // which handed the window, and everything typed into it, to whatever
    // answers on that port.
    //
    // `host()` rather than `hostname()` because it carries the port, and it
    // omits the default one on both sides — so `wss://example.com/ws` still
    // matches a page served from `https://example.com`.
    let Some(location) = web_sys::window().map(|window| window.location()) else {
        return Err(format!(
            "{host} is not this machine, and there is no page to compare it to"
        ));
    };
    let (page_scheme, page_host) = (
        location.protocol().unwrap_or_default(),
        location.host().unwrap_or_default(),
    );
    // A page's scheme decides which socket scheme is the same origin: an
    // `https:` page reaching `ws:` is a downgrade, and an `http:` page
    // reaching `wss:` is naming somewhere it did not come from.
    let expected = match page_scheme.as_str() {
        "https:" => "wss:",
        "http:" => "ws:",
        _ => "",
    };
    if !page_host.is_empty()
        && parsed.protocol() == expected
        && parsed.host().eq_ignore_ascii_case(&page_host)
    {
        return Ok(());
    }
    Err(format!(
        "{}//{} is neither this machine nor where this page came from",
        parsed.protocol(),
        parsed.host()
    ))
}

/// Where the media this daemon has cached can be fetched from.
///
/// The same origin as the socket, over HTTP: media never travels as a frame
/// (see [`crate::media_path`]), and where the two processes share no
/// filesystem the bytes have to come from somewhere. The daemon's web bridge
/// serves them beside the socket, so deriving one from the other is what
/// keeps a page from needing to be told twice.
#[must_use]
pub fn media_base_url() -> String {
    let socket = endpoint_url();
    // Through the parser rather than by trimming a suffix off the string. A
    // socket URL is allowed a query — the bridge routes on the path alone, so
    // `ws://host/ws?token=x` connects — and a suffix test then finds no `/ws`
    // to remove and produces `http://host/ws?token=x/media`, which asks the
    // socket endpoint for every photo.
    let Ok(parsed) = web_sys::Url::new(&socket) else {
        // `endpoint_url` returns only what already parsed, or the built-in
        // default; this is unreachable rather than a fallback.
        return format!(
            "http://127.0.0.1:{}{}",
            crate::DEFAULT_WEB_PORT,
            crate::WEB_MEDIA_PATH
        );
    };
    parsed.set_protocol(if parsed.protocol() == "wss:" {
        "https:"
    } else {
        "http:"
    });
    parsed.set_pathname(crate::WEB_MEDIA_PATH);
    // No query and no fragment: the key is joined onto this, so anything
    // here would land in the middle of the path. The token the media endpoint
    // also requires is appended after the key instead — see
    // [`media_token`].
    parsed.set_search("");
    parsed.set_hash("");
    // No trailing slash: `fetch_media` joins the key with one.
    parsed.href().trim_end_matches('/').to_string()
}

/// The token the media endpoint requires, as a query ready to append.
///
/// It is the *daemon's* token rather than the socket's, and the media
/// endpoint is behind the same check — so a request without it is a `404`
/// and every photo draws as a download nobody asked for. Empty when the page
/// was pointed at a daemon without one, which is a daemon that will refuse
/// the socket too: the failure belongs there, said once, rather than here per
/// photo.
#[must_use]
pub fn media_token() -> String {
    let Ok(parsed) = web_sys::Url::new(&endpoint_url()) else {
        return String::new();
    };
    parameter_of(&parsed.search(), "token")
        .map_or_else(String::new, |token| format!("?token={token}"))
}

/// One query parameter out of a query string.
///
/// The value is left encoded: it goes straight back into a URL.
fn parameter_of(search: &str, name: &str) -> Option<String> {
    search.trim_start_matches('?').split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

/// Whether this page is a preview rather than the deployment.
///
/// Declared by the page itself — a `<meta name="oxidezap-build" content="preview">`
/// the publisher puts there — and not guessed from the path, because the
/// consequence is too sharp for a guess. A preview shares its origin with the
/// deployment: same scheme, same host, same port, a different directory. That
/// was harmless while the page held nothing, and it is not now. A page that
/// runs its own session keeps the account in origin-scoped storage, and an
/// origin is not a directory — so unmerged code served under `/pr/<n>/` can
/// read the deployment's database, credentials and all, with no token
/// anywhere in the way.
///
/// Absent means not a preview, which is the safe direction: a deployment that
/// somehow lost the tag runs its own session as it should, and a preview that
/// somehow lost it is a preview nobody should have been pointing at an
/// account anyway.
///
/// The refusal it drives is a default rather than a wall — see
/// [`session_allowed_here`] — and it is worth being clear about what kind of
/// thing it is. It stops somebody wandering into a preview and pairing an
/// account there beside the deployment's. It is **not** a boundary: a preview
/// is built from its own branch's source, so that branch is free to delete
/// this check, and origin-scoped storage is readable by anything on the
/// origin regardless. What bounds that is who may publish a preview at all —
/// same-repository branches, which already require push access. See the
/// header of `.github/workflows/pages.yml`.
#[must_use]
pub fn is_preview() -> bool {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return false;
    };
    let Ok(Some(meta)) = document.query_selector("meta[name='oxidezap-build']") else {
        return false;
    };
    meta.get_attribute("content").as_deref() == Some("preview")
}

/// Whether this page may hold an account of its own.
///
/// Everything but a preview may. A preview may too, and only when somebody
/// asks for it in the URL — `#preview-session` — because the person testing
/// unmerged code on a preview is the one person who *wants* it to hold an
/// account, and refusing them outright makes the preview useless for the
/// thing it exists to preview.
///
/// The opt-in is what makes the default honest rather than absolute. Nobody
/// reaches this by following a link: the account a preview would share the
/// origin with is the deployment's, and someone who types the flag has said
/// they know whose database is one directory over. What it does not do is
/// make the two safe from each other — an origin is not a directory, and no
/// flag changes that. It moves the decision to a person.
#[must_use]
pub fn session_allowed_here() -> bool {
    !is_preview() || flag_present("preview-session")
}

/// A bare word in the fragment or the query, with no value after it.
///
/// [`find_parameter`] deliberately skips a pair it cannot split, because a
/// valueless `daemon` is a typo rather than a request. A flag is the opposite:
/// the word *is* the request, and `#preview-session=1` would be asking someone
/// to type a value that means nothing.
fn flag_present(name: &str) -> bool {
    let Some(location) = web_sys::window().map(|window| window.location()) else {
        return false;
    };
    let names = |text: String| {
        text.trim_start_matches(['#', '?'])
            .split('&')
            .any(|pair| pair == name)
    };
    location.hash().is_ok_and(names) || location.search().is_ok_and(names)
}

/// One query parameter off the page's own URL, or why there is none.
///
/// Three answers rather than two, for the same reason [`NamedDaemon`] has
/// three: "nobody wrote one" and "somebody wrote one this cannot read" lead
/// to opposite places. A truncated `%` in a pasted URL is the ordinary way to
/// arrive at the second, and collapsing it into the first started a session
/// against the browser's own store for somebody who had asked for a daemon.
enum Parameter {
    /// Not in the fragment or the query.
    Absent,
    /// There, and not decodable — a malformed percent escape.
    Unreadable,
    /// There, and decoded.
    Present(String),
}

/// One query parameter off the page's own URL.
fn query_parameter(name: &str) -> Option<String> {
    match read_parameter(name) {
        Parameter::Present(value) => Some(value),
        Parameter::Absent | Parameter::Unreadable => None,
    }
}

/// The same, keeping the distinction [`query_parameter`] discards.
fn read_parameter(name: &str) -> Parameter {
    let Some(window) = web_sys::window() else {
        return Parameter::Absent;
    };
    let location = window.location();

    // The fragment first, and it is where the answer is meant to be.
    //
    // A page's query string is sent to whoever served the page — it is in the
    // request line — so a token carried there reaches the static host's logs
    // before a single line of this runs. The fragment is never sent: browsers
    // strip it from the request, which is exactly why the implicit OAuth flow
    // used it for the same purpose.
    if let Ok(hash) = location.hash() {
        match find_parameter(hash.trim_start_matches('#'), name) {
            found @ (Parameter::Present(_) | Parameter::Unreadable) => return found,
            Parameter::Absent => {}
        }
    }

    // The query still answers, because refusing would not un-send it. What it
    // does is say so: the URL is already in somebody's logs, and the only
    // repair is a new token and a bookmark that uses `#`.
    let Ok(search) = location.search() else {
        return Parameter::Absent;
    };
    let found = find_parameter(search.trim_start_matches('?'), name);
    if matches!(found, Parameter::Present(_)) {
        log::warn!(
            "?{name}= was read from the query string, which the page's host has already been \
             sent. Put it after a `#` instead — and if it carried a token, draw a new one."
        );
    }
    found
}

/// One `key=value` out of an `&`-separated list.
fn find_parameter(pairs: &str, name: &str) -> Parameter {
    for pair in pairs.split('&') {
        // `continue`, not `?`. Returning from the whole function on the first
        // parameter without a value made `?debug&daemon=…` resolve to nothing
        // and fall silently back to the loopback default.
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == name {
            // Undecodable is an answer, not the absence of one: the name is
            // there and this is the value somebody meant.
            return match decode_component(value) {
                Some(decoded) => Parameter::Present(decoded),
                None => Parameter::Unreadable,
            };
        }
    }
    Parameter::Absent
}

/// Percent-decoding, through the browser's own decoder.
///
/// `decodeURIComponent` is right there and is the exact inverse of whatever
/// produced the URL; a hand-rolled one would be a second answer to a question
/// the platform has already answered.
fn decode_component(value: &str) -> Option<String> {
    js_sys::decode_uri_component(value)
        .ok()
        .and_then(|decoded| decoded.as_string())
}

/// Fetch one cached media payload from the daemon's bridge.
///
/// The web half of what `std::fs::read(media_path(key))` does natively. Same
/// key, same bytes, one HTTP round trip instead of a file read — which is
/// also why it is `async` where the native one is not, and why the front end
/// resolves media before it hands a frame on rather than inside it.
///
/// # Errors
///
/// A key the daemon does not hold, or a bridge that is not answering.
pub async fn fetch_media(base: &str, key: &str) -> Result<Vec<u8>, String> {
    fetch_media_within(base, key, MEDIA_TIMEOUT_MS, u64::MAX).await
}

/// Hand the daemon a payload it is about to be asked to send.
///
/// The mirror of [`fetch_media`], and the only direction a page writes. A
/// voice note exists only in the tab's memory until this lands, and the
/// request naming the key must not go out before it does, so the caller waits
/// on this rather than firing it alongside.
///
/// `PUT` because the key names the payload and staging it twice is the same
/// act twice. The bridge takes only `u-` keys, so this cannot reach the
/// daemon's own cache of what it fetched.
///
/// # Errors
///
/// The browser refused the request, the bridge refused the payload, or the
/// deadline passed.
pub async fn upload_media(base: &str, key: &str, bytes: &[u8]) -> Result<(), String> {
    use wasm_bindgen_futures::JsFuture;

    let window = web_sys::window().ok_or("no window to upload from")?;
    let url = format!(
        "{base}/{}{}",
        js_sys::encode_uri_component(key),
        media_token()
    );

    // Copied into a JS array rather than viewed: a view over wasm memory is
    // invalidated by any allocation the fetch machinery makes, and the body
    // outlives this call.
    let body = js_sys::Uint8Array::from(bytes);

    let abort = web_sys::AbortController::new()
        .map_err(|e| format!("could not arm an upload timeout: {e:?}"))?;
    let options = web_sys::RequestInit::new();
    options.set_method("PUT");
    options.set_body(&body);
    options.set_signal(Some(&abort.signal()));
    let _timeout = FetchDeadline::arm(&window, &abort, UPLOAD_TIMEOUT_MS)?;

    let response = JsFuture::from(window.fetch_with_str_and_init(&url, &options))
        .await
        .map_err(|e| format!("could not reach the daemon's media bridge: {e:?}"))?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| {
            "the media bridge answered with something that is not a response".to_string()
        })?;
    if !response.ok() {
        return Err(format!(
            "the daemon would not take that payload ({})",
            response.status()
        ));
    }
    Ok(())
}

/// Drop a payload the daemon staged for a send that is not going to run.
///
/// Best effort and unawaited by its caller: the send has already failed, and
/// what this prevents is a file nothing will read staying until the account
/// is wiped — staged uploads are deliberately spared by the cache sweep.
pub async fn discard_media(base: &str, key: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let url = format!(
        "{base}/{}{}",
        js_sys::encode_uri_component(key),
        media_token()
    );
    let options = web_sys::RequestInit::new();
    options.set_method("DELETE");
    if let Ok(promise) = window
        .fetch_with_str_and_init(&url, &options)
        .dyn_into::<js_sys::Promise>()
    {
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }
}

/// The same, under a deadline the caller chooses.
///
/// A frame's optional media and a download somebody asked for are not the
/// same errand. The default here is the short one, for the history load whose
/// thumbnails must not stall the stream; a requested attachment is promised a
/// minute, and capping each individual transfer at thirty seconds meant the
/// outer allowance was a fiction — the fetch was aborted, `Frames::apply`
/// found no bytes, and the code waiting reported a failure for something the
/// daemon had already cached.
///
/// # Errors
///
/// The browser refused the request, the bridge did not answer, or the
/// deadline passed.
pub async fn fetch_media_within(
    base: &str,
    key: &str,
    millis: i32,
    most: u64,
) -> Result<Vec<u8>, String> {
    use wasm_bindgen_futures::JsFuture;

    let window = web_sys::window().ok_or("no window to fetch from")?;
    let url = format!(
        "{base}/{}{}",
        js_sys::encode_uri_component(key),
        media_token()
    );

    // Bounded, because the caller resolves a frame's media before it hands
    // the frame on: a bridge that accepts the connection and never answers
    // would otherwise stall that frame for good, with no error to fall back
    // on. An abort turns it into an ordinary failure, which the renderer
    // already draws as an offer to download.
    let abort = web_sys::AbortController::new()
        .map_err(|e| format!("could not arm a fetch timeout: {e:?}"))?;
    let options = web_sys::RequestInit::new();
    options.set_signal(Some(&abort.signal()));
    let _timeout = FetchDeadline::arm(&window, &abort, millis)?;

    /// Aborts the request if this future is dropped before it finishes.
    ///
    /// The caller bounds a whole frame's media as well, and when *that*
    /// deadline wins it simply drops this future — which cancels nothing on
    /// its own, because dropping a `JsFuture` does not cancel the request
    /// behind it, and because [`FetchDeadline`]'s own drop *disarms* the
    /// abort rather than firing it. That is right for the path where the
    /// fetch already answered and wrong for this one, so the two are
    /// separate: one stops the timer, this one stops the request. Without it
    /// every frame that gave up left a browser connection and a daemon slot
    /// held by a request nobody was waiting for any more.
    ///
    /// Unconditional, because aborting a request that has already settled is
    /// a no-op — there is no success path worth disarming for.
    struct AbortOnDrop(web_sys::AbortController);

    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    let _abort_on_drop = AbortOnDrop(abort.clone());

    let response = JsFuture::from(window.fetch_with_str_and_init(&url, &options))
        .await
        .map_err(|e| format!("could not reach the daemon's media bridge: {e:?}"))?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| {
            "the media bridge answered with something that is not a response".to_string()
        })?;
    if !response.ok() {
        return Err(format!(
            "the daemon has no media under {key} ({})",
            response.status()
        ));
    }
    // Before the body is materialized, which is the only place the question
    // can be asked usefully: `array_buffer` allocates the whole payload, so a
    // caller that checks a budget *after* it has already spent what it was
    // trying not to. Dropping out here aborts the request too, through
    // `AbortOnDrop`.
    //
    // Best-effort by nature — a response may carry no `Content-Length`, and
    // then this cannot know until it has read it. That is not a hole worth
    // closing with a streaming reader here: the caller's own running total
    // still stops the *next* fetch, so what an absent length costs is one
    // payload of overshoot rather than an unbounded sequence.
    if let Some(length) = response
        .headers()
        .get("content-length")
        .ok()
        .flatten()
        .and_then(|value| value.trim().parse::<u64>().ok())
        && length > most
    {
        return Err(format!(
            "media {key} is {length} bytes, past the {most} this frame has left"
        ));
    }
    let buffer = JsFuture::from(
        response
            .array_buffer()
            .map_err(|e| format!("unreadable media body: {e:?}"))?,
    )
    .await
    .map_err(|e| format!("unreadable media body: {e:?}"))?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}
