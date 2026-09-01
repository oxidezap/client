//! The browser, as the library's three plugin traits.
//!
//! [`Runtime`] over `spawn_local` and `setTimeout`, [`HttpClient`] over
//! `fetch`, and [`Transport`] over `WebSocket`. The library is built for
//! this: on `wasm32` its traits are `async_trait(?Send)` and its
//! `MaybeSendSync` bound is empty, because a page is one thread.
//!
//! There is no `unsafe` here, and that is deliberate. This build runs real
//! workers — rebuilding the standard library with atomics is what the whole
//! `build-std` dance is for — so "a page is one thread" would be a false
//! premise to hang an `unsafe impl Send` on, however true it looks. A JS
//! object belongs to the agent that made it, and a type the compiler lets
//! anyone move is a type that will eventually be moved.
//!
//! So nothing here holds a JS object across a boundary the type system is
//! not already checking: the socket stays in the task that opened it and
//! callers hold a queue into it, which is the same arrangement `ipc::Link`
//! describes and for the same reason.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use bytes::Bytes;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::JsFuture;
use whatsapp_rust::wacore::net::{
    DisconnectReason, HttpClient, HttpRequest, HttpResponse, Transport, TransportEvent,
    TransportFactory,
};
use whatsapp_rust::wacore::runtime::{AbortHandle, Runtime};

/// The page's event loop, as a [`Runtime`].
pub struct BrowserRuntime;

impl Runtime for BrowserRuntime {
    /// Spawned on the page's microtask queue, and cancellable.
    ///
    /// `spawn_local` hands back nothing to cancel with, so the task races the
    /// future against a channel. A flag checked before the await would not
    /// do: a future that returns `Pending` is not polled again until
    /// something wakes it, so setting a flag would cancel nothing and the
    /// future would go on to run its side effects whenever it next woke.
    ///
    /// The abort is a value *sent*, never a sender dropped, and that is the
    /// whole of [`super::abort_requested`]: `AbortHandle` says which of its
    /// two endings happened by whether it calls this closure or drops it, so
    /// a closure that cancelled by being dropped made `.detach()` — the one
    /// call whose entire purpose is "run this to completion" — the thing
    /// that killed the task.
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + 'static>>) -> AbortHandle {
        let (cancel, cancelled) = futures_channel::oneshot::channel::<()>();
        oxidezap_platform::spawn(async move {
            // Which of the two ended it, said out loud. An abort here drops
            // the future *where it was*, and for a future that has not been
            // polled yet that means it never runs at all — no log, no error,
            // nothing. The library leans on exactly that (`set_media_task`
            // aborts a media task whose call is already gone, and the driver
            // task is written so an abort before its first poll releases the
            // call), so silence here is a call that disappears with no
            // account of itself anywhere. It costs one line per aborted task
            // and it is the difference between a report and a guess.
            let aborted = futures_lite::future::or(
                async {
                    future.await;
                    false
                },
                async {
                    super::abort_requested(cancelled).await;
                    true
                },
            )
            .await;
            if aborted {
                log::debug!("a spawned task was aborted before it finished");
            }
        });

        // No wrapper and no `unsafe`: `AbortHandle::new` wants a `Send`
        // closure, and a `oneshot::Sender<()>` is already one.
        AbortHandle::new(move || {
            // Nobody is listening once the task has finished, which is an
            // abort arriving late rather than a failure.
            let _ = cancel.send(());
        })
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(sleep(duration))
    }

    /// Inline: a page has no thread to move work to.
    ///
    /// Every caller of this is doing something the library considers
    /// CPU-bound, and on one thread the honest answer is to run it where it
    /// was asked for rather than to pretend it went elsewhere.
    fn spawn_blocking(&self, f: Box<dyn FnOnce() + 'static>) -> Pin<Box<dyn Future<Output = ()>>> {
        f();
        Box::pin(std::future::ready(()))
    }

    /// Always, because there is nowhere else for other work to run.
    fn yield_now(&self) -> Option<Pin<Box<dyn Future<Output = ()>>>> {
        Some(Box::pin(sleep(Duration::from_millis(0))))
    }

    /// Every item rather than every tenth: the loop that is yielding *is* the
    /// event loop, so ten items between yields is ten items of frozen page.
    fn yield_frequency(&self) -> u32 {
        1
    }
}

/// `setTimeout`, as a future.
///
/// [`oxidezap_platform::try_sleep`] rather than its `sleep`, and the
/// difference is the `false`: this is the library's own clock, so a wait that
/// cannot be armed — a worker torn down mid-task — must resolve rather than
/// park, because a future that never completes holds whatever is awaiting it
/// for the life of the page. Every other caller in this tree is a loop that
/// would spin instead, and takes the parking one.
async fn sleep(duration: Duration) {
    let _ = oxidezap_platform::try_sleep(duration).await;
}

/// `fetch`, as the library's [`HttpClient`].
///
/// Only [`HttpClient::execute`] is implemented, which is the only method the
/// trait requires: the streaming download and upload paths are declared
/// unsupported by their defaults, and a browser has no synchronous reader to
/// offer them anyway. Media is therefore buffered, which is what the page
/// already does for everything the daemon sent it.
///
/// What this cannot do is set `Origin`. It is a forbidden header name: the
/// browser writes the page's own origin and no API overrides it. So a request
/// from here is identifiable as not coming from WhatsApp Web, which is a
/// property of running in a browser at all rather than of this code.
pub struct BrowserHttpClient;

#[async_trait(?Send)]
impl HttpClient for BrowserHttpClient {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        let window = web_sys::window().ok_or_else(|| anyhow!("no window to fetch from"))?;

        let options = web_sys::RequestInit::new();
        options.set_method(&request.method);
        if let Some(body) = &request.body {
            // Copied into a JS array: handing `fetch` a view over wasm memory
            // would let a later allocation move the bytes out from under it.
            let bytes = js_sys::Uint8Array::from(body.as_ref());
            options.set_body(&bytes);
        }

        let headers = web_sys::Headers::new()
            .map_err(|e| anyhow!("the browser refused a header set: {e:?}"))?;
        for (name, value) in &request.headers {
            headers
                .append(name, value)
                .map_err(|e| anyhow!("the browser refused the header {name}: {e:?}"))?;
        }
        options.set_headers(&headers);

        let response = JsFuture::from(window.fetch_with_str_and_init(&request.url, &options))
            .await
            .map_err(|e| anyhow!("could not reach {}: {e:?}", request.url))?
            .dyn_into::<web_sys::Response>()
            .map_err(|_| anyhow!("the browser answered with something that is not a response"))?;

        // The status is read rather than turned into an error, because the
        // library reads it: a 401 or 403 means a stale media-auth token and a
        // 404 or 410 an expired URL, and both need a different retry from a
        // host-level failure. A client that hides them behind one error makes
        // every one of those retries repeat the same dead token.
        let status_code = response.status();

        let buffer = JsFuture::from(
            response
                .array_buffer()
                .map_err(|e| anyhow!("no body to read: {e:?}"))?,
        )
        .await
        .map_err(|e| anyhow!("could not read the body: {e:?}"))?;
        let body = js_sys::Uint8Array::new(&buffer).to_vec();

        Ok(HttpResponse { status_code, body })
    }
}

/// The browser's `WebSocket`, as the library's [`Transport`] — at one remove.
///
/// A dumb pipe for bytes, which is all the trait asks for: the framing above
/// it is WhatsApp's own and knows nothing about this.
///
/// What this holds is a queue, not a socket. A `web_sys::WebSocket` and its
/// closures belong to the agent that created them, and this build has real
/// worker threads, so a handle the compiler lets anyone move is a handle that
/// will eventually be moved onto one. The socket therefore stays in the task
/// that opened it and every writer posts to it — which is exactly what
/// `ipc::Link` does, and why: a type that owns JS objects cannot honestly be
/// `Send`, and a channel sender can.
///
/// Binary throughout. The socket is set to hand back `ArrayBuffer` rather than
/// `Blob`, because a `Blob` is read asynchronously and the read half here is a
/// callback that has to produce bytes on the spot.
pub struct BrowserTransport {
    outbound: async_channel::Sender<Outbound>,
}

/// What the owning task accepts.
enum Outbound {
    Send(Bytes),
    Close,
}

#[async_trait(?Send)]
impl Transport for BrowserTransport {
    async fn send(&self, data: Bytes) -> Result<(), anyhow::Error> {
        self.outbound
            .send(Outbound::Send(data))
            .await
            .map_err(|_| anyhow!("the socket to WhatsApp is closed"))
    }

    async fn disconnect(&self) {
        // Asked for rather than done here: closing needs the socket, and the
        // socket is the one thing this side does not have.
        let _ = self.outbound.send(Outbound::Close).await;
    }
}

/// Opens one socket per connection attempt, which is what the library asks of
/// a factory.
pub struct BrowserTransportFactory {
    url: String,
}

impl Default for BrowserTransportFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserTransportFactory {
    /// WhatsApp Web's own endpoint, which is where the library's native
    /// transport dials too.
    #[must_use]
    pub fn new() -> Self {
        Self {
            url: whatsapp_rust::wacore::net::WHATSAPP_WEB_WS_URL.to_string(),
        }
    }
}

#[async_trait(?Send)]
impl TransportFactory for BrowserTransportFactory {
    async fn create_transport(
        &self,
    ) -> Result<
        (
            std::sync::Arc<dyn Transport>,
            async_channel::Receiver<TransportEvent>,
        ),
        anyhow::Error,
    > {
        let socket = web_sys::WebSocket::new(&self.url)
            .map_err(|e| anyhow!("could not open {}: {e:?}", self.url))?;
        socket.set_binary_type(web_sys::BinaryType::Arraybuffer);

        // Unbounded, because the alternative is dropping frames the protocol
        // above cannot resynchronise without: this is the wire, not a queue of
        // requests. The events are consumed by the library's own read loop,
        // which is the same thread.
        let (tx, rx) = async_channel::unbounded();

        // Whether the socket has stopped being *pending* — opened, or given
        // up. One slot, because only the first of those matters and the
        // waiter is gone after it.
        let (settled, when_settled) = async_channel::bounded::<()>(1);

        let opened = {
            let tx = tx.clone();
            let settled = settled.clone();
            Closure::<dyn FnMut()>::new(move || {
                let _ = tx.try_send(TransportEvent::Connected);
                let _ = settled.try_send(());
            })
        };
        socket.set_onopen(Some(opened.as_ref().unchecked_ref()));

        let message = {
            let tx = tx.clone();
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                let data = event.data();
                let Some(buffer) = data.dyn_ref::<js_sys::ArrayBuffer>() else {
                    // Text where bytes were asked for is not something the
                    // framing above could read, so saying so beats handing it
                    // on to fail a layer up.
                    log::warn!("ignoring a non-binary frame from WhatsApp");
                    return;
                };
                let bytes = js_sys::Uint8Array::new(buffer).to_vec();
                let _ = tx.try_send(TransportEvent::DataReceived(Bytes::from(bytes)));
            })
        };
        socket.set_onmessage(Some(message.as_ref().unchecked_ref()));

        let closed = {
            let tx = tx.clone();
            let settled_on_close = settled.clone();
            Closure::<dyn FnMut(web_sys::CloseEvent)>::new(move |event: web_sys::CloseEvent| {
                // The code and the reason are carried through rather than
                // flattened: the library reads them to tell a routine stream
                // recycle from a failure, and logs the two differently.
                let _ = tx.try_send(TransportEvent::Disconnected(
                    DisconnectReason::ServerClose {
                        code: Some(event.code()),
                        reason: event.reason(),
                    },
                ));
                // A socket that closes without ever opening has to release
                // the writer below too, or it waits for an event that is
                // never coming and holds the socket for the life of the page.
                let _ = settled_on_close.try_send(());
            })
        };
        socket.set_onclose(Some(closed.as_ref().unchecked_ref()));

        let failed = {
            let tx = tx.clone();
            let settled_on_error = settled.clone();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                // A browser's socket error is deliberately opaque — a page
                // must not be able to probe the network with it — so there is
                // nothing to report but that it happened. `onclose` always
                // follows, and carries what little there is.
                let _ = tx.try_send(TransportEvent::Disconnected(DisconnectReason::Unknown));
                let _ = settled_on_error.try_send(());
            })
        };
        socket.set_onerror(Some(failed.as_ref().unchecked_ref()));

        // The socket never leaves this task, and the closures never leave
        // the socket. Everything a caller can hold is the sender below.
        let (outbound, orders) = async_channel::unbounded();
        oxidezap_platform::spawn(async move {
            // Moved in, not borrowed: this is where they die, which is what
            // keeps a reconnect from leaving four closures on the JS heap.
            let _handlers = (opened, message, closed, failed);

            // Nothing may be written until the socket is open. A browser
            // throws `InvalidStateError` on `send` while the state is
            // CONNECTING — it does not queue — and the library writes its
            // Noise ClientHello the moment the transport exists, which is the
            // same turn the socket was created in.
            //
            // This used to be a race that happened to be won: the library
            // fetched WhatsApp's `sw.js` for the client version first, and
            // that round trip was long enough for the socket to open
            // underneath it. Pinning the version took the fetch away and the
            // race started losing every time — the first write threw, the
            // loop below treated it as fatal, and the session reconnected
            // forever without ever putting a byte on the wire.
            let _ = when_settled.recv().await;
            if socket.ready_state() != web_sys::WebSocket::OPEN {
                // Never opened. The events above have already told the
                // library, which will retry; there is nothing here to drain.
                log::debug!("the socket to WhatsApp closed before it opened");
                socket.set_onopen(None);
                socket.set_onmessage(None);
                socket.set_onclose(None);
                socket.set_onerror(None);
                return;
            }

            while let Ok(order) = orders.recv().await {
                match order {
                    Outbound::Send(data) => {
                        // Copied out of linear memory, not viewed into it.
                        //
                        // `send_with_u8_array` hands the browser a
                        // `Uint8Array` *view* over the wasm heap, and this
                        // module is built with `--shared-memory` — so that
                        // heap is a `SharedArrayBuffer` and the view is a
                        // shared one. `WebSocket.send` refuses those by
                        // specification: "The provided ArrayBufferView value
                        // must not be shared." Every frame threw, the writer
                        // treated it as fatal, and the session reconnected
                        // forever without a byte reaching WhatsApp.
                        //
                        // `Uint8Array::from` allocates in the JavaScript heap
                        // and copies, so what goes out is unshared and the
                        // send is allowed. It costs one copy per frame, which
                        // for protocol traffic is nothing — and media takes
                        // the same route into the same rule.
                        //
                        // The same trap is waiting anywhere else a view is
                        // passed to a browser API: `fetch` bodies refuse
                        // shared views too. Every other crossing in this tree
                        // already goes through `Uint8Array::from`.
                        let bytes = js_sys::Uint8Array::from(&data[..]);
                        if let Err(e) = socket.send_with_array_buffer(&bytes.buffer()) {
                            // WhatsApp's socket, not the daemon's. This file is
                            // the library's transport; the daemon link is
                            // `oxidezap-ipc`, and a reader who trusts this
                            // sentence goes and reads the wrong one.
                            log::error!("could not write to the socket to WhatsApp: {e:?}");
                            break;
                        }
                    }
                    Outbound::Close => break,
                }
            }
            // The handlers come off first: a browser holding a reference to a
            // freed callback is a crash rather than a missed event, and the
            // close below would otherwise fire one on the way out.
            socket.set_onopen(None);
            socket.set_onmessage(None);
            socket.set_onclose(None);
            socket.set_onerror(None);
            let _ = socket.close();
        });

        Ok((std::sync::Arc::new(BrowserTransport { outbound }), rx))
    }
}
