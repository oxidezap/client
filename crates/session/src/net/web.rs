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
use wasm_bindgen_futures::{JsFuture, spawn_local};
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
    /// future against a channel the handle closes. A flag checked before the
    /// await would not do: a future that returns `Pending` is not polled
    /// again until something wakes it, so setting a flag would cancel
    /// nothing and the future would go on to run its side effects whenever
    /// it next woke. Dropping the sender wakes the receiver, which is what
    /// makes this an abort rather than a wish.
    fn spawn(&self, future: Pin<Box<dyn Future<Output = ()> + 'static>>) -> AbortHandle {
        let (cancel, cancelled) = futures_channel::oneshot::channel::<()>();
        spawn_local(async move {
            futures_lite::future::or(future, async move {
                // Resolves when the handle is dropped or aborted, and never
                // otherwise: the sender is only ever closed, never sent on.
                let _ = cancelled.await;
            })
            .await;
        });

        // No wrapper and no `unsafe`: `AbortHandle::new` wants a `Send`
        // closure, and a `oneshot::Sender<()>` is already one.
        AbortHandle::new(move || drop(cancel))
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
/// Resolves immediately where no timer can be armed — a worker torn down
/// mid-task — rather than never, because a future that never completes holds
/// whatever is awaiting it for the life of the page.
async fn sleep(duration: Duration) {
    /// Disarms the timer when the sleep is dropped.
    ///
    /// Load-bearing now that an abort really drops the future it raced: a
    /// `setTimeout` left armed fires into a `Closure` that has already been
    /// freed, which is a wasm-bindgen panic rather than a missed wakeup. And
    /// `yield_now` is this function at zero milliseconds, so a page that
    /// cancels anything in a loop would strand one timer per iteration.
    struct Timer {
        handle: i32,
        _fire: Closure<dyn FnMut()>,
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            if let Some(window) = web_sys::window() {
                window.clear_timeout_with_handle(self.handle);
            }
        }
    }

    let (tx, rx) = futures_channel::oneshot::channel::<()>();
    let Some(window) = web_sys::window() else {
        return;
    };
    let mut tx = Some(tx);
    let fire = Closure::<dyn FnMut()>::new(move || {
        if let Some(tx) = tx.take() {
            let _ = tx.send(());
        }
    });
    let Ok(handle) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
        fire.as_ref().unchecked_ref(),
        i32::try_from(duration.as_millis()).unwrap_or(i32::MAX),
    ) else {
        return;
    };
    // Held until it has fired *or this future is dropped*. `Closure::forget`
    // would hand it to the JS heap for the life of the page, and this is
    // called once per retry, per poll and per yield.
    let _timer = Timer {
        handle,
        _fire: fire,
    };
    let _ = rx.await;
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
            .map_err(|_| anyhow!("the daemon socket is closed"))
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

    /// Dial somewhere else — a relay, or a mock.
    #[must_use]
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
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

        let opened = {
            let tx = tx.clone();
            Closure::<dyn FnMut()>::new(move || {
                let _ = tx.try_send(TransportEvent::Connected);
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
            })
        };
        socket.set_onclose(Some(closed.as_ref().unchecked_ref()));

        let failed = {
            let tx = tx.clone();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                // A browser's socket error is deliberately opaque — a page
                // must not be able to probe the network with it — so there is
                // nothing to report but that it happened. `onclose` always
                // follows, and carries what little there is.
                let _ = tx.try_send(TransportEvent::Disconnected(DisconnectReason::Unknown));
            })
        };
        socket.set_onerror(Some(failed.as_ref().unchecked_ref()));

        // The socket never leaves this task, and the closures never leave
        // the socket. Everything a caller can hold is the sender below.
        let (outbound, orders) = async_channel::unbounded();
        spawn_local(async move {
            // Moved in, not borrowed: this is where they die, which is what
            // keeps a reconnect from leaving four closures on the JS heap.
            let _handlers = (opened, message, closed, failed);
            while let Ok(order) = orders.recv().await {
                match order {
                    Outbound::Send(data) => {
                        if let Err(e) = socket.send_with_u8_array(&data) {
                            log::error!("could not write to the daemon socket: {e:?}");
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
