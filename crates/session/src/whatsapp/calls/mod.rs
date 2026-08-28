//! Voice calls, which are the one thing the session does that a page cannot.
//!
//! Their media stack is `whatsapp_rust::voip`, whose codec is C and does not
//! build for `wasm32-unknown-unknown`. So this is the session's own platform
//! split, and it is the same shape as every other one in this tree: one set
//! of names, two implementations behind it, and no `cfg` in the session's
//! logic above.
//!
//! The browser side is not a stub that panics. A page still hears a call
//! ring — the signalling types are in `wacore` and arrive like any other
//! event — so it still records the call in the conversation, and still shows
//! it as missed when it stops ringing. What it cannot do is *answer*, and
//! that is refused where it is asked for rather than somewhere further in.

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(not(target_family = "wasm"))]
pub(super) use native::CallRegistry;

#[cfg(target_family = "wasm")]
mod web;
#[cfg(target_family = "wasm")]
pub(super) use web::CallRegistry;
