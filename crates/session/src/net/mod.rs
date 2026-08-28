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
