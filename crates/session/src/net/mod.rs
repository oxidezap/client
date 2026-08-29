//! How the session reaches the network.
//!
//! On a desktop it does not: the library's own default features supply a
//! Tokio WebSocket transport, a `ureq` HTTP client and a Tokio runtime, and
//! there is nothing here to choose between. A page has none of those — `mio`
//! does not build for `wasm32-unknown-unknown` and says so — but it has the
//! browser, which is a WebSocket, a `fetch` and an event loop already.
//!
//! So this module exists only on the web, and it is the answer to the three
//! things [`whatsapp_rust::Bot`]'s builder refuses to be finished without.
//! Every one of them is a `web-sys` binding written in Rust; none of them is
//! a JavaScript shim.

#[cfg(target_family = "wasm")]
pub mod web;

/// What a spawned task waits on to learn it has been aborted.
///
/// The library's [`AbortHandle`] boxes a closure and distinguishes its two
/// endings by whether that closure is ever *called*: `abort()` — which
/// `Drop` also does — calls it, and `detach()` drops it uncalled. So the
/// closure holds the sender, and this is the receiving half: a value sent
/// means abort, and a sender dropped means detached, which must go on
/// waiting forever rather than resolving.
///
/// Getting that backwards is not a subtle difference. The web runtime raced
/// every spawned future against `cancelled.await` and let a *dropped* sender
/// end the race, so `.detach()` — which exists to say "run this to
/// completion" — destroyed the closure holding the sender and cancelled the
/// task on the spot. On a desktop the same `detach()` drops a tokio
/// `JoinHandle`, which detaches, so only a page was affected: the library's
/// QR rotation is spawned and detached exactly like this, and a page acked
/// the server's `<pair-device>` and then showed no code at all.
///
/// [`AbortHandle`]: wacore::runtime::AbortHandle
#[cfg(any(target_family = "wasm", test))]
pub(crate) async fn abort_requested(cancelled: futures_channel::oneshot::Receiver<()>) {
    if cancelled.await.is_err() {
        // Detached. Nothing will ever abort this task, so the arm racing it
        // must never be the one that finishes.
        std::future::pending::<()>().await;
    }
}

/// Hand the bot builder whatever this platform has to supply.
///
/// On a desktop, nothing: the library's default cargo features already put a
/// Tokio transport, a `ureq` client and a Tokio runtime in place, and the
/// builder's typestate says as much. A page has none of those and gets the
/// three bindings in [`web`] instead.
///
/// One call at the one site that builds a bot, so the session's own code
/// never names a platform.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn with_platform_plugins<B>(
    builder: whatsapp_rust::bot::BotBuilder<
        B,
        whatsapp_rust::bot::Provided,
        whatsapp_rust::bot::Provided,
        whatsapp_rust::bot::Provided,
    >,
) -> whatsapp_rust::bot::BotBuilder<
    B,
    whatsapp_rust::bot::Provided,
    whatsapp_rust::bot::Provided,
    whatsapp_rust::bot::Provided,
> {
    builder
}

/// See the desktop half: here the three are ours to provide.
#[cfg(target_family = "wasm")]
pub(crate) fn with_platform_plugins<B>(
    builder: whatsapp_rust::bot::BotBuilder<
        B,
        whatsapp_rust::bot::MissingTransport,
        whatsapp_rust::bot::MissingHttpClient,
        whatsapp_rust::bot::MissingRuntime,
    >,
) -> whatsapp_rust::bot::BotBuilder<
    B,
    whatsapp_rust::bot::Provided,
    whatsapp_rust::bot::Provided,
    whatsapp_rust::bot::Provided,
> {
    builder
        .with_transport_factory(web::BrowserTransportFactory::new())
        .with_http_client(web::BrowserHttpClient)
        .with_runtime(web::BrowserRuntime)
}

#[cfg(test)]
mod tests {
    use super::abort_requested;
    use futures_lite::future::{block_on, or, pending, ready};

    /// An abort ends the task and a detach does not, which are the same
    /// closure being called and being dropped.
    ///
    /// Written as the race the runtime actually runs — the task's own future
    /// against its cancellation — because that composition is where the bug
    /// lived: the cancel arm winning is what killed a detached task, and a
    /// test of the receiver alone would not have seen it.
    #[test]
    fn a_detached_task_runs_on_and_an_aborted_one_stops() {
        // Detached: the handle dropped the closure without calling it.
        let (tell, told) = futures_channel::oneshot::channel();
        drop(tell);
        let task = or(pending::<&str>(), async {
            abort_requested(told).await;
            "cancelled"
        });
        assert_eq!(
            block_on(or(task, ready("still running"))),
            "still running",
            "a detached task was cancelled"
        );

        // Aborted: the closure was called, which is what `abort()` and the
        // handle's own `Drop` both do.
        let (tell, told) = futures_channel::oneshot::channel();
        tell.send(()).expect("the task is still listening");
        let task = or(pending::<&str>(), async {
            abort_requested(told).await;
            "cancelled"
        });
        assert_eq!(
            block_on(task),
            "cancelled",
            "an abort did not stop the task"
        );
    }
}
