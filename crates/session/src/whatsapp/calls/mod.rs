//! Voice and video calls.
//!
//! One implementation, where there were two. The browser half used to be a
//! set of refusals — "a call cannot be placed in a browser: there is no audio
//! codec here" — and that sentence was wrong about which thing was missing.
//! A page has a codec: MLow is pure Rust and lives in the library's own core,
//! which is what WhatsApp's clients negotiate anyway. What a page does not
//! have is a **UDP socket**, and that turned out to be the only thing in the
//! way.
//!
//! It supplies one now, in the shape a browser can: an `RTCPeerConnection` is
//! the same DTLS, SCTP and pre-negotiated DataChannel the native relay
//! transport assembles by hand, and the library takes it through
//! `Client::set_relay_transport_provider` (see [`crate::relay`]). The devices
//! came the same way — [`oxidezap_audio::open_call_audio`] and
//! `oxidezap_video` each grew their browser backend — so what is left here is
//! call logic, and call logic never had a platform in it.
//!
//! Two lines of this file's own are still the platform's, and both are about
//! *where work runs* rather than what it does: the session's executor
//! ([`crate::exec`]) rather than tokio's, since a page has no runtime to
//! reach for.

mod registry;

pub(super) use registry::CallRegistry;
