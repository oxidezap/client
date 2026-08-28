//! A call's video plane, or the absence of one.
//!
//! The same arrangement as [`crate::net`] and [`crate::exec`]: one set of
//! names, two implementations behind it, and no `cfg` in the session's own
//! logic above. What differs here is that the browser half is not a second
//! way of doing the same thing — it is the honest answer that a page cannot
//! do it at all. The camera is nokhwa (V4L2, AVFoundation, Media Foundation)
//! and the encoder is OpenH264, which is C.
//!
//! A browser has both, through `getUserMedia` and `VideoEncoder`, and neither
//! is bound yet. That is an API change rather than a backend swap — one is a
//! device the page must be granted, the other is asynchronous where this is
//! pulled — so what a page gets meanwhile is a camera that will not open,
//! reported where it is asked for.

#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

pub use platform::*;
