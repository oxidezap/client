//! A call's video plane.
//!
//! One implementation now, where there were two. The split existed because
//! the camera was nokhwa — V4L2, AVFoundation, Media Foundation — and the
//! encoder was OpenH264, which is C; a browser has neither, so the web half
//! was the names this module promises with an `open` that always refused, and
//! the comment above it said a page could not send a picture at all.
//!
//! It can. `getUserMedia` is the device and `VideoEncoder` is the codec, and
//! [`oxidezap_video`] now answers with either behind one name. What was a
//! platform split here is a platform split one crate down, which is where the
//! platform actually is.
//!
//! What remains of it in [`plane`] is two lines: where a pump runs
//! ([`crate::exec`]) and how it is stopped. A page's spawned task cannot be
//! aborted, so nothing is — teardown closes the channels the pumps read.

mod plane;

pub use plane::*;
