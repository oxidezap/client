//! The wire protocol between `oxidezapd` and its front ends.
//!
//! Types only: no sockets, no runtime. Both sides depend on this crate so a
//! protocol change breaks compilation rather than a running client.
//!
//! # Why a state version
//!
//! A client that connects mid-stream needs the current state and every event
//! after it, with nothing lost or applied twice. Taking a snapshot and *then*
//! subscribing drops whatever happens in between; subscribing and then
//! snapshotting delivers events the snapshot already reflects.
//!
//! Every mutation bumps [`StateVersion`]. The snapshot carries the version it
//! was taken at, each event carries the version it produced, and a client
//! discards events at or below its snapshot's version. The daemon can then
//! subscribe first and snapshot second, which loses nothing, and the duplicate
//! window resolves on the client with a comparison rather than a lock.

// A byte-stream endpoint is a thing only an operating system has. A page gets
// `web` instead, and everything above both gets [`Link`].
#[cfg(not(target_family = "wasm"))]
mod endpoint;
mod link;
mod protocol;
mod transport;
#[cfg(target_family = "wasm")]
pub mod web;
#[cfg(windows)]
pub mod windows_user;

#[cfg(not(target_family = "wasm"))]
pub use endpoint::{Endpoint, Reader, Writer};
pub use link::Link;
pub use protocol::{
    AccountIdentity, CallAction, ChatSummary, ClientRequest, ConnectionState, DaemonEvent,
    DaemonMessage, MessagePreview, PairingCode, ProtocolError, Request, RequestId, StateSnapshot,
    StateVersion,
};
pub use transport::{
    DEFAULT_WEB_PORT, PROTOCOL_VERSION, WEB_MEDIA_PATH, WEB_SOCKET_PATH, endpoint_path, lock_path,
    media_dir, media_path, state_dir,
};
