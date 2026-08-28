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

use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
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
pub fn connect(url: &str) -> Result<(Link, UnboundedReceiver<FromSocket>), String> {
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

    let (inbound, frames) = unbounded_channel::<FromSocket>();
    let (outbound, mut to_send) = unbounded_channel::<String>();

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
            let _ = inbound.send(FromSocket::Open);
        });
        socket.set_onopen(Some(opened.as_ref().unchecked_ref()));
        callbacks.push(opened);
    }

    let message_callback = {
        let inbound = inbound.clone();
        let message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            // Text only. The daemon speaks JSON and a binary frame would be
            // something else entirely — saying so beats decoding whatever it
            // is and failing to parse it a layer up.
            match event.data().as_string() {
                Some(line) => {
                    let _ = inbound.send(FromSocket::Line(line));
                }
                None => log::warn!("ignoring a non-text frame from the daemon"),
            }
        });
        socket.set_onmessage(Some(message.as_ref().unchecked_ref()));
        message
    };

    let close_callback = {
        let inbound = inbound.clone();
        let mut gone = Some(gone);
        let closed = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
            let reason = event.reason();
            let detail = if reason.is_empty() {
                format!("the daemon connection closed (code {})", event.code())
            } else {
                format!("the daemon connection closed: {reason}")
            };
            let _ = inbound.send(FromSocket::Closed(detail));
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
                let _ = inbound.send(FromSocket::Closed(
                    "the daemon accepted the connection but never opened it".to_string(),
                ));
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

    Ok((Link::over_socket(outbound), frames))
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
    let Some(asked) = named_daemon() else {
        return default();
    };
    asked
}

/// The daemon this page was pointed at, if it was pointed at one.
///
/// `None` is not an error and not a default: it is a page nobody told to
/// attach to anything, which is a page that runs its own session. Saying so
/// separately from [`endpoint_url`] is what lets a front end tell the two
/// apart — the URL alone cannot, because "nothing named" and "named the
/// usual place" are the same string.
#[must_use]
pub fn named_daemon() -> Option<String> {
    let asked = query_parameter("daemon")?;
    // A query parameter is whatever put the user on this page, which may be a
    // link somebody sent them. The daemon it names is handed the message
    // history and can be told to send, so an unchecked one turns a link into
    // a way to point the window at somebody else's server.
    match usable_endpoint(&asked) {
        Ok(()) => Some(asked),
        Err(why) => {
            // Redacted here too. A rejected URL is the *likeliest* one to be
            // pasted into an issue — it is the one that did not work — and it
            // carries the same token the accepted one does.
            log::error!("ignoring #daemon={}: {why}", without_secrets(&asked));
            None
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
    if is_loopback_host(&host) {
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

/// Whether a host names this machine.
///
/// A parsed hostname, so there is no port and no userinfo left to be confused
/// by — and `localhost.example.com` is simply a different string.
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
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

/// One query parameter off the page's own URL.
fn query_parameter(name: &str) -> Option<String> {
    let location = web_sys::window()?.location();

    // The fragment first, and it is where the answer is meant to be.
    //
    // A page's query string is sent to whoever served the page — it is in the
    // request line — so a token carried there reaches the static host's logs
    // before a single line of this runs. The fragment is never sent: browsers
    // strip it from the request, which is exactly why the implicit OAuth flow
    // used it for the same purpose.
    if let Ok(hash) = location.hash()
        && let Some(found) = find_parameter(hash.trim_start_matches('#'), name)
    {
        return Some(found);
    }

    // The query still answers, because refusing would not un-send it. What it
    // does is say so: the URL is already in somebody's logs, and the only
    // repair is a new token and a bookmark that uses `#`.
    let found = find_parameter(location.search().ok()?.trim_start_matches('?'), name)?;
    log::warn!(
        "?{name}= was read from the query string, which the page's host has already been sent. \
         Put it after a `#` instead — and if it carried a token, draw a new one."
    );
    Some(found)
}

/// One `key=value` out of an `&`-separated list.
fn find_parameter(pairs: &str, name: &str) -> Option<String> {
    for pair in pairs.split('&') {
        // `continue`, not `?`. Returning from the whole function on the first
        // parameter without a value made `?debug&daemon=…` resolve to nothing
        // and fall silently back to the loopback default.
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == name {
            return decode_component(value);
        }
    }
    None
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
    fetch_media_within(base, key, MEDIA_TIMEOUT_MS).await
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
pub async fn fetch_media_within(base: &str, key: &str, millis: i32) -> Result<Vec<u8>, String> {
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
    let buffer = JsFuture::from(
        response
            .array_buffer()
            .map_err(|e| format!("unreadable media body: {e:?}"))?,
    )
    .await
    .map_err(|e| format!("unreadable media body: {e:?}"))?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}
