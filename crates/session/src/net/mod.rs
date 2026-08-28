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
