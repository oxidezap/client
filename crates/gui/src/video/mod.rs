//! Video module for video message playback
//!
//! This module provides:
//! - MP4 demuxing and H.264 decoding, by openh264 or by the browser
//! - Memory-efficient streaming decoder (on-demand frame decoding, ~16x less memory)
//! - Video player state management
//! - Audio extraction from video files (for video audio track playback)
//! - A live call's video, decoded per direction on threads of its own
//!
//! Everything platform-split here is split the one way: a module name, and a
//! `#[cfg_attr(path)]` pair naming the file each target reads it from. There
//! were three spellings of that once — a bare `#[cfg] mod`, a duplicate `mod`
//! item with a `#[path]` on the second, and a `mod`-plus-`use ... as` alias —
//! which is how the browser's decoder came to be called `unsupported`.

/// The audio track inside a video file, and the SPS below it: both are the
/// decoder's, so on the web — where there is none — they compile and nothing
/// reaches them.
#[cfg_attr(target_family = "wasm", allow(dead_code, reason = "no decoder here"))]
mod audio;
/// A live call's two directions: on the desktop, decoded on threads of their
/// own; on a page, by the browser, which is asynchronous already.
#[cfg_attr(target_family = "wasm", path = "call_web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "call_native.rs")]
mod call;
/// Getting H.264 out of an MP4, for whichever decoder reads it.
mod demux;
/// What every decoded picture obeys, whichever decoder produced it.
mod geometry;
mod player;
#[cfg_attr(target_family = "wasm", allow(dead_code, reason = "no decoder here"))]
mod sps;
/// The browser's own H.264 decoder, standing in for openh264.
#[cfg(target_family = "wasm")]
mod webcodecs;

/// A video attachment, decoded a frame at a time: `openh264` on the desktop,
/// the browser's `VideoDecoder` on a page. The container work above both of
/// them is [`demux`], so this is a decoder swap rather than a second reader.
#[cfg_attr(target_family = "wasm", path = "web.rs")]
#[cfg_attr(not(target_family = "wasm"), path = "native.rs")]
mod platform;

// Memory-efficient streaming decoder (on-demand decoding, ~3MB vs ~48MB)
pub use platform::StreamingVideoDecoder;

// A live call's two directions, decoded off the IPC thread.
pub use call::{CallFrame, CallVideo, FrameSink, LatestFrames};

// Video player state machine
pub use player::{VideoPlayer, VideoPlayerState};

/// Build a decoder wherever this platform can build one.
///
/// The desktop's is plain Rust and belongs on the background executor:
/// parsing a container and walking its samples is real work and holds up a
/// frame if it runs on the UI thread. The browser's holds JS objects — a
/// `VideoDecoder` and the closures it calls back through — which exist on the
/// thread that made them, and gpui's background executor is a real worker
/// there. So the same call is off-thread on one and inline on the other, and
/// the caller does not carry a `cfg` for it.
#[cfg(not(target_family = "wasm"))]
pub async fn build_decoder(
    cx: &mut gpui::AsyncApp,
    data: std::sync::Arc<Vec<u8>>,
) -> anyhow::Result<StreamingVideoDecoder> {
    use gpui::AppContext as _;
    cx.background_spawn(async move { StreamingVideoDecoder::new(&data) })
        .await
}

/// The same, with only the half that has to be here left here.
///
/// The demux and the AAC decode are plain Rust over the whole file, twice,
/// and a long attachment opened on the window thread stops scrolling for as
/// long as they take. Only the `VideoDecoder` itself is bound to this thread,
/// so the expensive half goes to the background executor and comes back as a
/// [`demux::Track`] the decoder is built from here.
#[cfg(target_family = "wasm")]
pub async fn build_decoder(
    cx: &mut gpui::AsyncApp,
    data: std::sync::Arc<Vec<u8>>,
) -> anyhow::Result<StreamingVideoDecoder> {
    use gpui::AppContext as _;
    let track = cx
        .background_spawn(async move { demux::Track::read(&data) })
        .await?;
    StreamingVideoDecoder::attach(track)
}
