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

/// Which client version to announce, where this side has to decide it.
///
/// `None` means "let the library find out", which is what a desktop does: it
/// fetches `https://web.whatsapp.com/sw.js` once a day and reads
/// `client_revision` out of it.
///
/// A page cannot. That request is a cross-origin `fetch` and WhatsApp sends
/// no `Access-Control-Allow-Origin`, so the browser blocks it before it goes
/// out and the session never gets past "failed to resolve app version",
/// retrying forever behind its backoff. Measured on the deployed page rather
/// than guessed — and the socket to `wss://web.whatsapp.com/ws/chat` opens
/// from that same origin quite happily, because a WebSocket upgrade is not
/// subject to the same-origin policy and a `fetch` is.
///
/// `no-cors` is not a way round it: it yields an opaque response whose body
/// cannot be read, and the body is the whole point. A proxy is the other way,
/// and it would mean this bundle needs a server — the one thing the web build
/// exists not to need.
pub(crate) async fn app_version() -> Option<(u32, u32, u32)> {
    platform_version().await
}

#[cfg(not(target_family = "wasm"))]
async fn platform_version() -> Option<(u32, u32, u32)> {
    None
}

#[cfg(target_family = "wasm")]
async fn platform_version() -> Option<(u32, u32, u32)> {
    Some(web::app_version().await)
}

/// `2.3000.1046291534-alpha` and the like, down to three numbers.
///
/// Anything after the third is dropped: the feed marks pre-release builds
/// with a suffix, and the wire wants the triple. Parsed strictly rather than
/// leniently, because the value comes from somewhere this code does not
/// control — a field that is not three integers is a feed to ignore, not a
/// number to guess at.
///
/// Here rather than beside its one caller in [`web`], so that it can be
/// tested: a test inside a `wasm32`-only module is a test that runs nowhere.
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
fn parse_version(named: &str) -> Option<(u32, u32, u32)> {
    let digits = named.split('-').next()?;
    let mut parts = digits.split('.');
    let mut triple = [0u32; 3];
    for slot in &mut triple {
        *slot = parts.next()?.parse::<u32>().ok()?;
    }
    // A fourth component would mean this is not the shape it was taken for.
    parts
        .next()
        .is_none()
        .then_some((triple[0], triple[1], triple[2]))
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn a_release_version_reads_back_as_three_numbers() {
        assert_eq!(
            parse_version("2.3000.1045368834"),
            Some((2, 3000, 1045368834))
        );
    }

    /// What the feed actually serves today, suffix and all.
    #[test]
    fn a_prerelease_suffix_is_dropped() {
        assert_eq!(
            parse_version("2.3000.1046291534-alpha"),
            Some((2, 3000, 1046291534))
        );
    }

    #[test]
    fn anything_that_is_not_three_numbers_is_refused() {
        for bad in ["", "2.3000", "2.3000.1.4", "2.3000.x", "latest", "-alpha"] {
            assert_eq!(parse_version(bad), None, "{bad} should not parse");
        }
    }
}
