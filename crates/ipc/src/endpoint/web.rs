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
        format!(
            "could not open a socket to {url}: {}",
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
        // The callbacks live as long as the socket does, and the socket lives
        // as long as the page. Dropping the `Closure` would leave the browser
        // calling into freed memory, which is what `forget` is for here.
        opened.forget();
    }

    {
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
        message.forget();
    }

    {
        let inbound = inbound.clone();
        let closed = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
            let reason = event.reason();
            let detail = if reason.is_empty() {
                format!("the daemon connection closed (code {})", event.code())
            } else {
                format!("the daemon connection closed: {reason}")
            };
            let _ = inbound.send(FromSocket::Closed(detail));
        });
        socket.set_onclose(Some(closed.as_ref().unchecked_ref()));
        closed.forget();
    }

    {
        // `onerror` carries nothing useful in a browser — the event is
        // deliberately opaque, so a page cannot probe the network with it —
        // and `onclose` always follows. Logging it is all there is to do.
        let failed = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            log::warn!("the daemon socket reported an error; waiting for the close");
        });
        socket.set_onerror(Some(failed.as_ref().unchecked_ref()));
        failed.forget();
    }

    // The one place the socket is written from. Everything else queues.
    spawn_local(async move {
        while let Some(frame) = to_send.recv().await {
            if is_open.get() {
                if let Err(e) = socket.send_with_str(&frame) {
                    log::error!("could not reach the daemon: {e:?}");
                    break;
                }
            } else {
                held.borrow_mut().push(frame);
            }
        }
        // The sender was dropped, or a write failed: either way this
        // connection is over, and a socket left open would keep the daemon
        // holding a client slot for it.
        let _ = socket.close();
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
    query_parameter("daemon").unwrap_or_else(|| {
        format!(
            "ws://127.0.0.1:{}{}",
            crate::DEFAULT_WEB_PORT,
            crate::WEB_SOCKET_PATH
        )
    })
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
    let http = if let Some(rest) = socket.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = socket.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        socket
    };
    let base = http
        .strip_suffix(crate::WEB_SOCKET_PATH)
        .unwrap_or(&http)
        .trim_end_matches('/')
        .to_string();
    format!("{base}{}", crate::WEB_MEDIA_PATH)
}

/// One query parameter off the page's own URL.
fn query_parameter(name: &str) -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    for pair in search.trim_start_matches('?').split('&') {
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
    use wasm_bindgen_futures::JsFuture;

    let window = web_sys::window().ok_or("no window to fetch from")?;
    let url = format!("{base}/{}", js_sys::encode_uri_component(key));
    let response = JsFuture::from(window.fetch_with_str(&url))
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
