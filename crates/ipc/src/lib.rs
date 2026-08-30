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

// Every client-side transport lives under `endpoint`, whatever it is made of:
// a Unix socket, a Windows named pipe, or — where the front end is a page — a
// WebSocket. That is the whole of the platform split on this side (see
// /AGENTS.md); everything above it gets [`Link`] and never mentions any of
// them.
mod endpoint;
/// Where a frame ends, and how long one may be — the framing this crate
/// exists to put around the domain types.
pub mod framing;
mod link;
mod protocol;
mod transport;
#[cfg(windows)]
pub mod windows_user;

#[cfg(target_family = "wasm")]
pub use endpoint::web;
#[cfg(not(target_family = "wasm"))]
pub use endpoint::{Endpoint, Hangup, Reader, Writer};
pub use framing::{FrameRead, MAX_DAEMON_FRAME_BYTES, MAX_REQUEST_BYTES, read_frame};
pub use link::Link;
pub use protocol::{
    AccountIdentity, CallAction, ChatSummary, ClientRequest, ConnectionState, DaemonEvent,
    DaemonMessage, MessagePreview, PageCursor, PairingCode, ProtocolError, Request, RequestId,
    StateSnapshot, StateVersion,
};
pub use transport::{
    DEFAULT_WEB_PORT, PROTOCOL_VERSION, STAGED_PREFIX, WEB_MEDIA_PATH, WEB_SOCKET_PATH,
    endpoint_path, is_staged_key, lock_path, media_dir, media_path, staged_key, state_dir,
    web_token_path,
};
