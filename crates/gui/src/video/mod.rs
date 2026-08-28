//! Video module for video message playback
//!
//! This module provides:
//! - MP4 demuxing and H.264 software decoding via OpenH264
//! - Memory-efficient streaming decoder (on-demand frame decoding, ~16x less memory)
//! - Video player state management
//! - Audio extraction from video files (for video audio track playback)
//! - A live call's video, decoded per direction on threads of its own

mod audio;
mod call;
mod player;
mod sps;
mod streaming;

// Memory-efficient streaming decoder (on-demand decoding, ~3MB vs ~48MB)
pub use streaming::StreamingVideoDecoder;

// A live call's two directions, decoded off the IPC thread.
pub use call::{CallFrame, CallVideo, FrameSink, LatestFrames};

// Video player state machine
pub use player::{VideoPlayer, VideoPlayerState};
