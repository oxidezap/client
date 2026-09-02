//! A live call's video, decoded by the browser.
//!
//! The same names as the desktop half beside it and a different engine
//! underneath, so nothing above learns which build it is in. What differs is
//! not only the codec: the desktop gives each direction a thread and a short
//! queue, and a page has neither to give. It does not need them — `VideoDecoder` is
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

use wacore::voip::h264::{au_has_idr, nal_unit_type, split_annexb};

use super::geometry::{Rotation, declares_unreadably};
use super::webcodecs;

/// The SPS and PPS an access unit carries, if it carries both.
///
/// Owned for the PPS because the caller wants the two together and the
/// borrow of the second outlives the iterator that found the first.
fn parameter_sets(access_unit: &[u8]) -> Option<(&[u8], Vec<u8>)> {
    let mut sps = None;
    let mut pps = None;
    for nal in split_annexb(access_unit) {
        match nal_unit_type(nal) {
            7 => sps = sps.or(Some(nal)),
            8 => pps = pps.or(Some(nal.to_vec())),
            _ => {}
        }
    }
    Some((sps?, pps?))
}

/// Bytes as the hex a capture is compared against.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

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

/// How many units may sit in the browser's decode queue before frames are
/// dropped instead of fed.
///
/// The desktop path gives each direction a four-frame queue; this is that
/// bound moved to the far side of the binding, where the queue actually is.
const MAX_QUEUED_UNITS: u32 = 4;

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
    /// Whether the wait for a keyframe has already been reported. One line
    /// per wait, not one per unit refused while waiting.
    waiting: std::cell::Cell<bool>,
    /// Stamps the units, since a call's frames carry no presentation time of
    /// their own and a decoder wants them monotonic.
    fed: std::cell::Cell<i32>,
    /// Whether this stream's shape has been said once. See [`Stream::describe`].
    described: std::cell::Cell<bool>,
}

impl Stream {
    fn new(call_id: String, stream: VideoStream, frames: FrameSink) -> Self {
        Self {
            call_id,
            stream,
            frames,
            decoder: RefCell::new(None),
            started: std::cell::Cell::new(false),
            waiting: std::cell::Cell::new(false),
            fed: std::cell::Cell::new(0),
            described: std::cell::Cell::new(false),
        }
    }

    /// Say what this stream's units are made of, once.
    ///
    /// The one thing a pane that draws nothing cannot tell you: whether the
    /// bitstream is the shape the decoder was configured for. `voip-cli`
    /// prints exactly this and it is how the peer's stream was established as
    /// decodable while ours was not -- there, the two lines sit side by side
    /// and differ. Here every other line said video was fine.
    fn describe(&self, frame: &CallVideoFrame) {
        if self.described.get() {
            return;
        }
        let nals: SmallVec<[u8; 8]> = split_annexb(&frame.data).map(nal_unit_type).collect();
        // Only once a parameter set has been seen: before that there is
        // nothing to name the decoder's configuration with, and saying so
        // early would spend the one line on the least useful unit.
        let sets = parameter_sets(&frame.data);
        if let Some((sps, pps)) = sets {
            self.described.set(true);
            log::debug!(
                "the {:?} stream carries avc1.{} ({} byte(s), NALs {:?}, SPS {}, PPS {})",
                self.stream,
                hex(sps.get(1..4).unwrap_or_default()),
                frame.data.len(),
                nals,
                hex(sps),
                hex(&pps),
            );
        }
    }

    /// Something upstream lost units, so what the decoder holds no longer
    /// matches what the sender encoded against.
    fn interrupted(&self) {
        self.abandon();
    }

    /// Give up the reference chain, and everything the decoder is still
    /// holding on its behalf.
    ///
    /// Clearing `started` stops this side feeding; it does nothing about the
    /// units the browser has already taken. Their pictures still arrive, and
    /// each is a frame from before the break drawn over the pane while this
    /// side waits for the keyframe meant to replace them. The reset empties
    /// that queue and moves the generation on, so the copies in flight are
    /// recognised as belonging to the stream being left.
    fn abandon(&self) {
        if let Some(decoder) = self.decoder.borrow().as_ref() {
            decoder.reset();
        }
        self.started.set(false);
    }

    fn accept(&self, frame: CallVideoFrame) {
        self.describe(&frame);
        if frame.gap {
            self.abandon();
        }
        if !self.started.get() {
            // The bitstream, not only the flag beside it. `voip-cli` restarts
            // on `au_has_idr` and recovers where this waited: a unit that
            // carries an IDR *is* a recovery point whatever the flag says, and
            // a flag that is wrong once costs the pane every picture until the
            // sender's next keyframe -- which the peer sent four times in a
            // whole call. Widened rather than replaced: a keyframe the sender
            // vouches for is still one.
            if !frame.keyframe && !au_has_idr(&frame.data) {
                // The one silent refusal on this path, and the one that
                // costs a whole call: a stream that never receives a
                // keyframe waits here for every unit and draws nothing,
                // which reads in a log exactly like a stream that received
                // nothing at all. Said once per wait, not per unit.
                if !self.waiting.replace(true) {
                    log::debug!(
                        "the {:?} stream is waiting for a keyframe before it can decode",
                        frame.stream
                    );
                }
                return;
            }
            self.waiting.set(false);
            self.started.set(true);
        }

        // Read before the decoder is handed it, because a decoder allocates
        // its reference and output buffers from the parameter set — from
        // numbers the *peer* chose. A unit carrying no parameter set declares
        // no new geometry and is left alone.
        if let super::sps::Geometry::Size(width, height) = super::sps::coded_size(&frame.data)
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
        // The same rule as the budget above and a different sentence: a set
        // the parser gave up on is a picture the decoder allocates from with
        // nothing having checked it.
        if declares_unreadably(&frame.data) {
            log::warn!(
                "refusing a video stream on call {} whose geometry cannot be read",
                self.call_id
            );
            self.started.set(false);
            return;
        }

        let mut held = self.decoder.borrow_mut();
        if held.is_none() {
            // The first keyframe is what carries the sets, so it is also what
            // the decoder can first be built from. Refused for this keyframe
            // rather than for good: a later one may carry a set this browser
            // will take.
            match self.build(&frame) {
                Some(decoder) => *held = Some(decoder),
                None => {
                    self.started.set(false);
                    return;
                }
            }
        }
        let Some(decoder) = held.as_ref() else {
            return;
        };

        // A decoder that has stopped is one whose pictures will never come, so
        // it is dropped and the next keyframe builds another. If *this* frame
        // is a keyframe it is that one: returning here instead would discard
        // the recovery point and leave the pane blank for a whole group of
        // pictures, waiting for the keyframe after it.
        if let Some(e) = decoder.failure() {
            log::debug!("the {:?} video of a call stopped: {e}", self.stream);
            *held = None;
            self.started.set(false);
            if !frame.keyframe {
                return;
            }
            self.started.set(true);
            match self.build(&frame) {
                Some(decoder) => *held = Some(decoder),
                None => {
                    self.started.set(false);
                    return;
                }
            }
        }
        let Some(decoder) = held.as_ref() else {
            return;
        };

        // The browser's decode queue is unbounded and a call is a stream that
        // does not wait: a browser decoding slower than the peer encodes
        // would bank compressed units for the length of the call, drawing a
        // picture further behind with every one. Dropped rather than queued,
        // which is what every other queue on this path does, and the drop
        // costs the reference chain, so the stream waits for the next
        // keyframe exactly as it does after a gap.
        if decoder.queued() >= MAX_QUEUED_UNITS {
            log::debug!(
                "dropping a {:?} call frame: the browser's decoder is behind",
                self.stream
            );
            // The same act a gap is: what the browser already holds is worth
            // nothing now, and drawing it would put stale pictures on the
            // pane. See `abandon`, which this cannot call because the
            // decoder is borrowed here.
            decoder.reset();
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

    /// Build a decoder from the parameter sets this keyframe carries.
    ///
    /// One place rather than two, because the first build and the rebuild
    /// after a failure differ only in what the caller does with `started`.
    fn build(&self, frame: &CallVideoFrame) -> Option<webcodecs::Decoder> {
        match webcodecs::Decoder::with_budget(
            &frame.data,
            Rotation::to_upright(frame.orientation),
            MAX_PIXELS,
            Some(self.sink()),
        ) {
            Ok(decoder) => Some(decoder),
            Err(e) => {
                log::warn!("no decoder for the {:?} video of a call: {e}", self.stream);
                None
            }
        }
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
