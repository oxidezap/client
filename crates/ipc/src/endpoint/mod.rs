//! Connecting to the daemon, on whatever transport this front end has.
//!
//! Two of them are byte streams a process opens — a Unix socket and a Windows
//! named pipe — and live in [`stream`]. The third is a WebSocket, which is
//! what a page has instead of either, and lives in [`web`].
//!
//! This module and `daemon/listener/` are the whole of the platform split
//! (see /AGENTS.md): a transport is added *here*, so that the framing, the
//! requests and the protocol above them stay written once. What the three
//! share on the way out is [`crate::Link`]; what they do not share is the way
//! in, because a process parks a thread in a read and a page is handed a
//! callback, and pretending those are one shape would cost more than it saves.

/// The transports an operating system provides.
#[cfg(not(target_family = "wasm"))]
mod stream;
/// The transport a browser tab provides.
#[cfg(target_family = "wasm")]
pub mod web;

#[cfg(not(target_family = "wasm"))]
pub use stream::{Endpoint, Hangup, Reader, Writer};

/// Whether a host names this machine.
///
/// A parsed hostname, so there is no port and no userinfo left to be confused
/// by — and `localhost.example.com` is simply a different string.
///
/// The address half goes through `IpAddr::is_loopback` rather than a list of
/// literals, because that is the test the *daemon* applies to the address it
/// was told to bind: `--web 127.0.0.2:9527` is accepted there and the whole
/// of `127.0.0.0/8` is loopback. Recognising only `127.0.0.1` here left a
/// bridge the person had deliberately enabled unreachable from the browser
/// while satisfying the daemon's own boundary. One rule, applied on both
/// sides.
/// Its one caller is the web endpoint; the tests below are why it lives here.
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
pub(crate) fn is_loopback_host(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    // Unwrapped, because `Url::hostname` hands back an IPv6 literal without
    // its brackets while a hand-written host may keep them.
    let named = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    named
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::is_loopback_host;

    /// Here rather than beside its one caller in [`web`], for the reason a
    /// `wasm32`-only test is no test at all: it runs nowhere.
    #[test]
    fn every_loopback_address_names_this_machine() {
        for host in [
            "localhost",
            "127.0.0.1",
            // The rest of `127.0.0.0/8`, which the daemon accepts as a bind
            // address and this used to refuse as a destination.
            "127.0.0.2",
            "127.1.2.3",
            "::1",
            "[::1]",
        ] {
            assert!(is_loopback_host(host), "{host} is loopback");
        }
    }

    #[test]
    fn nothing_else_does() {
        for host in [
            "",
            "example.com",
            // A name that merely starts the same way. The whole point of
            // testing the parsed hostname is that this is a different string.
            "localhost.example.com",
            "127.0.0.1.example.com",
            "10.0.0.1",
            "0.0.0.0",
            "192.168.1.10",
            "2001:db8::1",
        ] {
            assert!(!is_loopback_host(host), "{host} is not loopback");
        }
    }
}
