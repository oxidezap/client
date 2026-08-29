//! Video, on a platform with no H.264 decoder in the build.
//!
//! The desktop decodes video itself: `mp4` demuxes the container and
//! `openh264` turns the H.264 track into frames. The second half is C, and
//! `wasm32-unknown-unknown` has no C toolchain behind it — so this build has
//! a demuxer and nothing to hand the samples to.
//!
//! Rather than let that surface as a decode that fails halfway through a
//! clip, the decoder refuses at construction, which the player already treats
//! as an error state: the bubble keeps its thumbnail and says the video
//! cannot be played here. That is the same path a corrupt file takes.
//!
//! # The way out
//!
//! A browser has a hardware H.264 decoder and exposes it as WebCodecs
//! (`web_sys::VideoDecoder`), which is bindable from Rust with no JavaScript.
//! It is asynchronous where this API is synchronous — frames arrive on a
//! callback rather than being pulled by index — so adopting it is a change to
//! [`VideoPlayer`](super::VideoPlayer)'s shape rather than a second
//! implementation of this one, and it is left as its own piece of work.

use std::time::Duration;

use anyhow::Result;
use gpui::RenderImage;

use super::audio::VideoAudio;

/// One decoded frame. Never constructed here; the type exists so the player
/// above it is the same code on both platforms.
pub struct StreamingFrame {
    pub image: std::sync::Arc<RenderImage>,
    pub timestamp: Duration,
    pub index: usize,
}

/// The decoder that is not in this build.
pub struct StreamingVideoDecoder {
    /// Uninhabited in practice: `new` never returns one.
    _private: (),
}

impl StreamingVideoDecoder {
    /// Refuse, and say why.
    ///
    /// # Errors
    ///
    /// Always. See the module documentation.
    pub fn new(_mp4_data: &[u8]) -> Result<Self> {
        anyhow::bail!("this build has no H.264 decoder, so video cannot be played in the browser")
    }

    #[must_use]
    pub fn frame_count(&self) -> usize {
        0
    }

    #[must_use]
    pub fn duration(&self) -> Duration {
        Duration::ZERO
    }

    pub fn seek(&mut self, _time: Duration) {}

    pub fn seek_to_frame(&mut self, _target_index: usize) {}

    #[must_use]
    pub fn current_frame(&self) -> Option<&StreamingFrame> {
        None
    }

    pub fn reset(&mut self) {}

    pub fn take_audio(&mut self) -> Option<VideoAudio> {
        None
    }
}
