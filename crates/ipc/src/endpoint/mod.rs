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
pub use stream::{Endpoint, Reader, Writer};
