//! A live call's video, decoded by the browser.
//!
//! The same names as [`super::call`] and a different engine underneath, so
//! nothing above learns which build it is in. What differs is not only the
//! codec: the desktop gives each direction a thread and a short queue, and a
//! page has neither to give. It does not need them — `VideoDecoder` is
//! already asynchronous, so the work a thread was there to move off the
//! caller happens off it anyway.
//!
//! Every rule the desktop path obeys is obeyed here, because they are about
//! the stream rather than about threads:
//!
//! * A decoder born mid-stream waits for a keyframe. Nothing before one means
//!   anything, and feeding it produces a second of torn picture over the last
//!   good one.
//! * A gap makes it wait again, since what follows references units that
//!   never arrived.
//! * The peer's parameter set is read *before* the decoder sees it, because a
//!   decoder allocates from numbers the peer chose.
//! * A peer's orientation describes their device, so drawing upright means
//!   undoing it rather than applying it again.
//!
//! # Where this works
//!
//! Attached to an `oxidezapd`, which is where calls happen at all: a page
//! holding its own session cannot answer one, so the frames this decodes are
//! the ones a daemon is already sending it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::RenderImage;
use oxidezap_core::{CallVideoFrame, VideoStream};
use smallvec::SmallVec;

use super::geometry::Rotation;
use super::webcodecs;

/// Where a decoded picture goes.
///
/// Without the `Send + Sync` its desktop twin carries: that bound is there
/// for the decode threads, and nothing here runs on one.
pub type FrameSink = Arc<dyn Fn(CallFrame)>;

/// One decoded picture, and which side of the call it is.
pub struct CallFrame {
    pub call_id: String,
    pub stream: VideoStream,
    pub image: Arc<RenderImage>,
}

/// The newest decoded picture of each direction.
///
/// A slot per direction rather than a queue, for the reason its desktop twin
/// is one: a picture that could not be drawn when it arrived is worth nothing
/// once the next has come.
#[derive(Clone, Default)]
pub struct LatestFrames {
    newest: Rc<RefCell<SmallVec<[CallFrame; 2]>>>,
}

impl LatestFrames {
    /// Hold this picture, replacing whatever that direction had.
    pub fn put(&self, frame: CallFrame) {
        let mut held = self.newest.borrow_mut();
        if let Some(slot) = held.iter_mut().find(|held| held.stream == frame.stream) {
            *slot = frame;
        } else {
            held.push(frame);
        }
    }

    /// Take what has arrived since the last look.
    pub fn take(&self) -> SmallVec<[CallFrame; 2]> {
        std::mem::take(&mut *self.newest.borrow_mut())
    }
}

/// The largest picture a call frame may be.
///
/// The dimensions come off a peer's bitstream, so their product is somebody
/// else's number: 4K is far past anything a call offers and still bounds the
/// allocation. The same number the desktop path uses, and for the same
/// reason.
const MAX_PIXELS: usize = 3840 * 2160;

/// Both directions of a call, each decoded as its units arrive.
pub struct CallVideo {
    call_id: String,
    local: Stream,
    remote: Stream,
}

impl CallVideo {
    pub fn new(call_id: String, frames: FrameSink) -> Self {
        Self {
            local: Stream::new(call_id.clone(), VideoStream::Local, Arc::clone(&frames)),
            remote: Stream::new(call_id.clone(), VideoStream::Remote, frames),
            call_id,
        }
    }

    /// Which call this is decoding, so a frame for a different one is
    /// recognised as the call having moved on rather than fed to a decoder
    /// mid-bitstream.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Something between here and the camera dropped units.
    ///
    /// Both directions, because the channel that lost them carries both and a
    /// gap in it says nothing about which was in flight.
    pub fn interrupted(&self) {
        self.local.interrupted();
        self.remote.interrupted();
    }

    /// Hand one access unit to the decoder that owns its direction.
    pub fn accept(&self, frame: CallVideoFrame) {
        match frame.stream {
            VideoStream::Local => self.local.accept(frame),
            VideoStream::Remote => self.remote.accept(frame),
        }
    }
}

/// One direction: its decoder, and whether it may be fed yet.
struct Stream {
    call_id: String,
    stream: VideoStream,
    frames: FrameSink,
    /// Built on the first keyframe rather than at construction: a call's
    /// parameter sets arrive with the picture, and there is nothing to
    /// configure a decoder from before one has.
    decoder: RefCell<Option<webcodecs::Decoder>>,
    /// Whether the decoder holds a reference chain worth continuing. Cleared
    /// by a gap and by any refusal, and regained at the next keyframe.
    started: std::cell::Cell<bool>,
    /// Stamps the units, since a call's frames carry no presentation time of
    /// their own and a decoder wants them monotonic.
    fed: std::cell::Cell<i32>,
}

impl Stream {
    fn new(call_id: String, stream: VideoStream, frames: FrameSink) -> Self {
        Self {
            call_id,
            stream,
            frames,
            decoder: RefCell::new(None),
            started: std::cell::Cell::new(false),
            fed: std::cell::Cell::new(0),
        }
    }

    /// Something upstream lost units, so what the decoder holds no longer
    /// matches what the sender encoded against.
    fn interrupted(&self) {
        self.started.set(false);
    }

    fn accept(&self, frame: CallVideoFrame) {
        if frame.gap {
            self.started.set(false);
        }
        if !self.started.get() {
            if !frame.keyframe {
                return;
            }
            self.started.set(true);
        }

        // Read before the decoder is handed it, because a decoder allocates
        // its reference and output buffers from the parameter set — from
        // numbers the *peer* chose. A unit carrying no parameter set declares
        // no new geometry and is left alone.
        if let Some((width, height)) = super::sps::coded_size(&frame.data)
            && (width as usize).saturating_mul(height as usize) > MAX_PIXELS
        {
            log::warn!(
                "refusing a {width}x{height} video stream on call {}",
                self.call_id
            );
            // Not a gap to recover from: every unit that follows references
            // this picture, so the stream stays refused until the peer sends
            // a parameter set describing one that fits.
            self.started.set(false);
            return;
        }

        let mut held = self.decoder.borrow_mut();
        if held.is_none() {
            // The first keyframe is what carries the sets, so it is also what
            // the decoder can first be built from.
            match webcodecs::Decoder::with_budget(
                &frame.data,
                Rotation::to_upright(frame.orientation),
                MAX_PIXELS,
                Some(self.sink()),
            ) {
                Ok(decoder) => *held = Some(decoder),
                Err(e) => {
                    log::warn!("no decoder for the {:?} video of a call: {e}", self.stream);
                    // Refused for this keyframe, not for good: a later one may
                    // carry a set this browser will take.
                    self.started.set(false);
                    return;
                }
            }
        }
        let Some(decoder) = held.as_ref() else {
            return;
        };

        // A decoder that has stopped is one whose pictures will never come;
        // the next keyframe builds a new one rather than feeding a dead one.
        if let Some(e) = decoder.failure() {
            log::debug!("the {:?} video of a call stopped: {e}", self.stream);
            *held = None;
            self.started.set(false);
            return;
        }

        // Their device, not their picture: drawing it upright is undoing the
        // turn rather than repeating it.
        decoder.set_rotation(Rotation::to_upright(frame.orientation));
        let stamp = self.fed.get();
        self.fed.set(stamp.wrapping_add(1));
        decoder.decode(&frame.data, stamp, frame.keyframe);
    }

    /// Where this direction's pictures go once they are decoded.
    fn sink(&self) -> Rc<dyn Fn(webcodecs::Picture)> {
        let call_id = self.call_id.clone();
        let stream = self.stream;
        let frames = Arc::clone(&self.frames);
        Rc::new(move |picture: webcodecs::Picture| {
            frames(CallFrame {
                call_id: call_id.clone(),
                stream,
                image: picture.image,
            });
        })
    }
}
