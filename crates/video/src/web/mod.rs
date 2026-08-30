//! The camera and the encoder a browser has.
//!
//! The desktop opens a device through nokhwa — V4L2, AVFoundation, Media
//! Foundation — and encodes with OpenH264, which is C. A page has neither and
//! needs neither: `getUserMedia` is the device and `VideoEncoder` is the
//! codec, and both are the browser's own.
//!
//! What leaves this module is what leaves the desktop half: Annex-B H.264
//! access units, one [`EncodedFrame`] each, on a channel the session hands
//! straight to the library. Nothing above learns which backend produced them,
//! which is the whole point — a peer cannot tell either.
//!
//! # Annex-B is asked for, not assembled
//!
//! `VideoEncoder` emits AVCC by default: length-prefixed NALs with the
//! parameter sets carried out of band, in the chunk's metadata. The library's
//! video source wants Annex-B with start codes, and converting between them
//! is a demuxer nobody should have to write twice — so the encoder is
//! configured with `avc: { format: "annexb" }` and emits what is wanted. The
//! parameter sets then ride in front of every IDR, which is also what the
//! peer's decoder needs to allocate from.
//!
//! # Why a `<video>` element and a timer
//!
//! A `MediaStreamTrack` does not hand out frames. The direct route is
//! `MediaStreamTrackProcessor`, which is a `ReadableStream` of `VideoFrame`s
//! and exists in one browser family; the portable route is to attach the
//! stream to a `<video>` and construct a `VideoFrame` from the element, which
//! is what this does. The cost is that pacing is ours rather than the
//! device's: the timer runs at the quality's frame rate and takes whatever
//! the element is showing, so a camera delivering fewer frames than asked for
//! encodes duplicates rather than stalling. That is the right failure for a
//! call — the RTP stride the library paces by is fixed at negotiation, and a
//! stream that simply stops is a frozen picture with no gap marked.

mod camera;

pub use camera::{CameraControl, CameraStream, is_available, open_camera};
