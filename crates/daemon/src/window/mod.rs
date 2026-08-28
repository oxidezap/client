//! Making sure there is a window, from the side that does not own one.
//!
//! The tray's Open and a client's `ShowWindow` both mean the same thing: the
//! user wants the interface up. Whoever already has a window raises it, and
//! that half is the same everywhere — it is a message on the signal channel,
//! and every attached front end reads it.
//!
//! What differs is what happens when *nobody* answers. A daemon beside a
//! desktop can start one, which is the mirror of the front end starting a
//! daemon it could not find. A page cannot: there is no second process to
//! launch, and the tab that would raise itself is the only window there is.
//! So the browser half stops after the message, which is not a stub — it is
//! the whole of what "make sure there is a window" can mean there.

#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

pub use platform::show;
