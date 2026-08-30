//! Video module for video message playback
//!
//! This module provides:
//! - MP4 demuxing and H.264 software decoding via OpenH264
//! - Memory-efficient streaming decoder (on-demand frame decoding, ~16x less memory)
//! - Video player state management
//! - Audio extraction from video files (for video audio track playback)
//! - A live call's video, decoded per direction on threads of its own

mod audio;
/// A live call's two directions, decoded on threads of their own.
#[cfg(not(target_family = "wasm"))]
mod call;
/// The same names where the decoder and the threads are both missing.
#[cfg(target_family = "wasm")]
#[path = "call_unsupported.rs"]
mod call;
/// Getting H.264 out of an MP4, for whichever decoder reads it.
mod demux;
/// What every decoded picture obeys, whichever decoder produced it.
mod geometry;
mod player;
mod sps;
/// The browser's own H.264 decoder, standing in for openh264.
#[cfg(target_family = "wasm")]
mod webcodecs;

/// The real decoder: `mp4` for the container, `openh264` for the picture.
#[cfg(not(target_family = "wasm"))]
mod streaming;
/// The same API where the second of those cannot be built.
#[cfg(target_family = "wasm")]
mod unsupported;

#[cfg(target_family = "wasm")]
use unsupported as streaming;

// Memory-efficient streaming decoder (on-demand decoding, ~3MB vs ~48MB)
pub use streaming::StreamingVideoDecoder;

/// Whether this build can decode a video at all.
///
/// True on both now: the desktop links openh264 and a page uses the browser's
/// own decoder through WebCodecs. It stays as a constant rather than being
/// deleted because it answers *before the bytes are fetched* — a build with
/// no decoder should not make the daemon download a film to refuse it — and
/// because a browser that turns out to have no `VideoDecoder` still refuses
/// at construction, which is a per-file answer this cannot give.
pub const CAN_DECODE: bool = true;

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

/// The same, on the one thread a page has. See the note above.
#[cfg(target_family = "wasm")]
pub async fn build_decoder(
    _cx: &mut gpui::AsyncApp,
    data: std::sync::Arc<Vec<u8>>,
) -> anyhow::Result<StreamingVideoDecoder> {
    StreamingVideoDecoder::new(&data)
}
