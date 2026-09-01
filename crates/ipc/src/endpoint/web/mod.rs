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
//! # Three files, and only one of them is the transport
//!
//! [`socket`] is it: the connection, its two queues and the [`crate::Link`]
//! callers hold. That is what /AGENTS.md keeps in this directory.
//!
//! [`media`] is the sideband beside it — the payload a frame names rather
//! than carries, fetched, staged or discarded over HTTP because the two ends
//! share no filesystem. It is not a transport; it is the web half of what
//! `std::fs::read(media_path(key))` does natively, and it lives here rather
//! than in the daemon's crate because this is the *client* side of it.
//!
//! [`address`] is what the page was told: which daemon to attach to, whether
//! this page may hold an account at all, and the parameters both are read
//! from. Both of the others ask it, which is the reason it is neither's.
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

/// What this page was told about a daemon, and about itself.
mod address;
/// The payload a frame names, over HTTP.
mod media;
/// The transport.
mod socket;

// One module to the front end, whatever it is divided into here: these names
// are `oxidezap_ipc::web::*`, which front ends outside this workspace compile
// against, so the division above is not allowed to be visible in them.
pub use address::{
    NamedDaemon, endpoint_url, is_preview, named_daemon, session_allowed_here, without_secrets,
};
pub use media::{
    discard_media, fetch_media, fetch_media_within, media_base_url, media_token, upload_media,
};
pub use socket::{FromSocket, Inbound, connect};
