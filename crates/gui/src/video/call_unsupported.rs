//! A live call's video, where there is no decoder to give it a picture.
//!
//! The same names as [`super::call`] and the same shape, so nothing above
//! this learns which build it is in — the arrangement `streaming` and
//! `unsupported` already have beside it, and for the same cause: the decoder
//! is OpenH264, which is C, and the threads a decode runs on are threads a
//! `wasm32-unknown-unknown` page does not have.
//!
//! So the frames are dropped where they arrive rather than somewhere further
//! in. A page that accumulated access units for a decoder that will never
//! exist would be spending a call's whole bandwidth to fill memory, and the
//! card that would draw them already asks [`super::CAN_DECODE`].

use std::sync::Arc;

use gpui::RenderImage;
use oxidezap_core::{CallVideoFrame, VideoStream};
use smallvec::SmallVec;

/// Where a decoded picture would go.
///
/// Without the `Send + Sync` its desktop twin carries: that bound is there
/// for the decode threads, and nothing here runs on one.
pub type FrameSink = Arc<dyn Fn(CallFrame)>;

/// One decoded picture, and which side of the call it is.
///
/// Constructed nowhere in this build. It exists because the window's code is
/// written once and still has to name what it would draw.
pub struct CallFrame {
    pub call_id: String,
    pub stream: VideoStream,
    pub image: Arc<RenderImage>,
}

/// The newest decoded picture of each direction — always none of them.
#[derive(Clone, Default)]
pub struct LatestFrames;

impl LatestFrames {
    /// Nothing decodes here, so nothing arrives to be held.
    pub fn put(&self, _frame: CallFrame) {}

    /// Always empty, which is what the window draws when a call has no
    /// picture — the same as a call whose peer has their camera off.
    pub fn take(&self) -> SmallVec<[CallFrame; 2]> {
        SmallVec::new()
    }
}

/// Both directions of a call that cannot be drawn.
pub struct CallVideo {
    call_id: String,
}

impl CallVideo {
    pub fn new(call_id: String, _frames: FrameSink) -> Self {
        Self { call_id }
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Dropped, not queued: see the module note.
    pub fn accept(&self, _frame: CallVideoFrame) {}

    /// Nothing is holding a reference frame to invalidate.
    pub fn interrupted(&self) {}
}
