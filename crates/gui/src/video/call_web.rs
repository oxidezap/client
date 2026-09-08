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

use super::recovery::h264::{MAX_PIXELS, nal_unit_type, split_annexb};

use super::geometry::Rotation;
use super::webcodecs;

use super::recovery::{Recovery, RecoverySink};

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
    #[cfg(test)]
    pub fn take(&self) -> SmallVec<[CallFrame; 2]> {
        std::mem::take(&mut *self.newest.borrow_mut())
    }

    /// Leave pictures whose enabling state has not reached the UI in their slots.
    pub fn take_for(&self, calls: &oxidezap_core::CallState) -> SmallVec<[CallFrame; 2]> {
        let mut ready = SmallVec::new();
        let Some(call) = calls.active() else {
            return ready;
        };
        let mut held = self.newest.borrow_mut();
        let mut index = 0;
        while index < held.len() {
            let frame = &held[index];
            if frame.call_id == call.call_id && call.video.is_on(frame.stream) {
                ready.push(held.remove(index));
            } else {
                index += 1;
            }
        }
        ready
    }

    pub fn clear(&self, stream: VideoStream) {
        self.newest
            .borrow_mut()
            .retain(|frame| frame.stream != stream);
    }
}

/// How many units may sit in the browser's decode queue before frames are
/// dropped instead of fed.
///
/// The desktop path gives each direction a four-frame queue; this is that
/// bound moved to the far side of the binding, where the queue actually is.
const MAX_QUEUED_UNITS: u32 = 4;

/// One direction of a call, decoded as its units arrive.
pub struct CallVideo {
    call_id: String,
    stream: VideoStream,
    decoder: Stream,
}

impl CallVideo {
    pub fn new(
        call_id: String,
        stream: VideoStream,
        frames: FrameSink,
        recover: RecoverySink,
    ) -> Self {
        Self {
            decoder: Stream::new(call_id.clone(), stream, frames, recover),
            call_id,
            stream,
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
    /// Wait for a keyframe rather than rendering with missing references.
    pub fn interrupted(&self) {
        self.decoder.interrupted();
    }

    /// Hand one access unit to the decoder that owns its direction.
    pub fn accept(&self, frame: CallVideoFrame) {
        if frame.call_id != self.call_id || frame.stream != self.stream {
            return;
        }
        self.decoder.accept(frame);
    }
}

/// One direction: its decoder, and whether it may be fed yet.
struct Stream {
    recovery: Rc<Recovery>,
    access_units: RefCell<super::recovery::h264::AccessUnits>,
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
    /// The orientation bits the pane is currently drawing with. Said when it
    /// moves: a camera switch that changes only the sender's rotation bits is
    /// otherwise invisible in the log, and the picture keeps the old turn.
    applied: std::cell::Cell<Option<u8>>,
}

impl Stream {
    fn new(call_id: String, stream: VideoStream, frames: FrameSink, recover: RecoverySink) -> Self {
        Self {
            recovery: Rc::new(Recovery::new(call_id.clone(), stream, recover)),
            access_units: RefCell::new(super::recovery::h264::AccessUnits::default()),
            call_id,
            stream,
            frames,
            decoder: RefCell::new(None),
            started: std::cell::Cell::new(false),
            waiting: std::cell::Cell::new(false),
            fed: std::cell::Cell::new(0),
            described: std::cell::Cell::new(false),
            applied: std::cell::Cell::new(None),
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
                "the {:?} stream carries avc1.{} ({} byte(s), NALs {:?}, SPS bytes {}, PPS bytes {})",
                self.stream,
                hex(sps.get(1..4).unwrap_or_default()),
                frame.data.len(),
                nals,
                sps.len(),
                pps.len(),
            );
        }
    }

    /// Something upstream lost units, so what the decoder holds no longer
    /// matches what the sender encoded against.
    fn interrupted(&self) {
        self.abandon();
        self.recovery.request();
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

    fn accept(&self, mut frame: CallVideoFrame) {
        self.describe(&frame);
        if frame.gap {
            self.abandon();
        }
        let (data, recovers) = match self.access_units.borrow_mut().prepare(&frame.data) {
            Ok(Some(picture)) => picture,
            Ok(None) => {
                if !self.started.get() {
                    self.recovery.request();
                }
                return;
            }
            Err(_) => {
                self.abandon();
                self.recovery.request();
                return;
            }
        };
        if let std::borrow::Cow::Owned(data) = data {
            frame.data = data;
        }
        let mut recovering = !self.started.get();
        if !self.started.get() {
            if !recovers {
                self.recovery.request();
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
                    self.recovery.request();
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
            recovering = true;
            log::debug!("the {:?} video of a call stopped: {e}", self.stream);
            *held = None;
            self.started.set(false);
            if !recovers {
                self.recovery.request();
                return;
            }
            self.started.set(true);
            match self.build(&frame) {
                Some(decoder) => *held = Some(decoder),
                None => {
                    self.started.set(false);
                    self.recovery.request();
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
            self.recovery.request();
            return;
        }

        // Their device, not their picture: drawing it upright is undoing the
        // turn rather than repeating it.
        let rotation = Rotation::to_upright(frame.orientation);
        if self.applied.get() != Some(frame.orientation) {
            self.applied.set(Some(frame.orientation));
            log::debug!(
                "the {:?} stream draws orientation bits {} as {:?}",
                frame.stream,
                frame.orientation,
                rotation,
            );
        }
        decoder.set_rotation(rotation);
        let stamp = self.fed.get();
        self.fed.set(stamp.wrapping_add(1));
        decoder.decode(&frame.data, stamp, recovers);
        if decoder.failure().is_some() {
            self.started.set(false);
            self.recovery.request();
        } else if recovering {
            self.recovery.admitted();
        }
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
            Ok(decoder) => {
                if log::log_enabled!(log::Level::Debug) {
                    let call_id = self.call_id.clone();
                    let stream = self.stream;
                    let first_stamp = self.fed.get();
                    let last_report = std::cell::Cell::new(wacore::time::Instant::now());
                    decoder.enable_diagnostics(move |stats, final_report| {
                        if !log::log_enabled!(log::Level::Debug) {
                            return;
                        }
                        let now = wacore::time::Instant::now();
                        let elapsed = now.saturating_duration_since(last_report.get());
                        if !final_report && elapsed < std::time::Duration::from_secs(5) {
                            return;
                        }
                        last_report.set(now);
                        log::debug!(
                            "video readback call={call_id} stream={stream:?} first_stamp={first_stamp} final={final_report} interval_ms={} totals={stats:?}",
                            elapsed.as_millis(),
                        );
                    });
                }
                Some(decoder)
            }
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
        let recovery = self.recovery.clone();
        Rc::new(move |picture: webcodecs::Picture| {
            recovery.output();
            frames(CallFrame {
                call_id: call_id.clone(),
                stream,
                image: picture.image,
            });
        })
    }
}
