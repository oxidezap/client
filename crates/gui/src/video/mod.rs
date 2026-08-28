//! Video module for video message playback
//!
//! This module provides:
//! - MP4 demuxing and H.264 software decoding via OpenH264
//! - Memory-efficient streaming decoder (on-demand frame decoding, ~16x less memory)
//! - Video player state management
//! - Audio extraction from video files (for video audio track playback)

mod audio;
mod player;

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
/// Asked before the bytes are fetched, not after. The decoder refuses at
/// construction where there is none — but by then the daemon has downloaded
/// the whole file from WhatsApp and pushed it across the loopback, which for
/// a large clip is a long wait for an answer that was known at compile time.
pub const CAN_DECODE: bool = cfg!(not(target_family = "wasm"));

// Video player state machine
pub use player::{VideoPlayer, VideoPlayerState};
