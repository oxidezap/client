//! A live call's video, decoded for the window.
//!
//! The daemon sends access units because that is what crosses a socket
//! cheaply; this is where they become something GPUI can draw. Two streams
//! arrive — the peer's and our own — and each gets a thread and a decoder of
//! its own, because a decoder is a state machine over one bitstream and
//! feeding it two would produce nothing either side could use.
//!
//! Off the IPC thread, deliberately: that thread also carries history loads
//! and reads every photo they name off disk, and a call would otherwise put a
//! frame's decode in front of the conversation the user is scrolling.
//!
//! The queue in front of each decoder is short and dropped from rather than
//! blocked on: a decoder that has fallen behind should skip to the newest
//! unit, not walk a backlog it will never draw. What a drop costs is the
//! reference chain — every unit after it points at one this decoder never
//! saw — so the decoder is told, and waits for a keyframe rather than
//! rendering a second of torn macroblocks over the last good picture.
//!
//! Told *on the first unit after the gap*, not by a flag beside the queue: a
//! gap is a position in a stream, and a decoder that read a flag while
//! dequeuing something from before the gap would clear it and walk straight
//! into the unit the flag was about.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::RenderImage;
use image::{Frame, RgbaImage};
use openh264::decoder::{DecodedYUV, Decoder};
use openh264::formats::YUVSource as _;
use oxidezap_core::{CallVideoFrame, VideoStream};
use smallvec::SmallVec;

use super::streaming::{Rotation, swap_rb_in_place, write_bgra_rotated};

/// Where a decoded picture goes.
///
/// A closure rather than a channel of this module's own: what the window
/// carries frames in is the window's business, and a leaf that named the
/// front end's event type would be pointing the wrong way. Called from a
/// decode thread, so it may not block.
pub type FrameSink = Arc<dyn Fn(CallFrame) + Send + Sync>;

/// The newest decoded picture of each direction, waiting for the window.
///
/// A slot per direction rather than a place in a queue, because a decoded
/// frame is 3.5 MiB of pixels and the only one worth drawing is the last one.
/// The window's event channel is hundreds of messages deep — it has to be, for
/// the messages that may not be lost — and a call that outran a stalled window
/// would fill it with obsolete pictures: gigabytes of them, and every state
/// frame behind ten seconds of video nobody will see. Here the newest picture
/// replaces the one before it and the channel carries only a nudge.
#[derive(Clone, Default)]
pub struct LatestFrames {
    /// Indexed by direction: two slots, and no key to get wrong.
    slots: Arc<std::sync::Mutex<[Option<CallFrame>; 2]>>,
}

fn slot_of(stream: VideoStream) -> usize {
    match stream {
        VideoStream::Local => 0,
        VideoStream::Remote => 1,
    }
}

impl LatestFrames {
    /// Hold this picture for the window, dropping whatever that direction was
    /// holding: it is a frame the window never drew and never will.
    pub fn put(&self, frame: CallFrame) {
        let mut slots = self.slots.lock().expect("call frame slots poisoned");
        let slot = slot_of(frame.stream);
        slots[slot] = Some(frame);
    }

    /// Everything waiting, in one pass, leaving the slots empty.
    pub fn take(&self) -> SmallVec<[CallFrame; 2]> {
        let mut slots = self.slots.lock().expect("call frame slots poisoned");
        slots.iter_mut().filter_map(Option::take).collect()
    }
}

/// One decoded picture, and which side of the call it is.
pub struct CallFrame {
    pub call_id: String,
    pub stream: VideoStream,
    pub image: Arc<RenderImage>,
}

/// Both directions of the call being drawn.
///
/// Created when the first frame of a call arrives and dropped when the call
/// does, which is what closes the threads: a decoder held past its call would
/// keep a megabyte of reference frames for a picture nobody is looking at.
pub struct CallVideo {
    call_id: String,
    local: Stream,
    remote: Stream,
}

impl CallVideo {
    pub fn new(call_id: String, frames: FrameSink) -> Self {
        Self {
            local: Stream::spawn(call_id.clone(), VideoStream::Local, Arc::clone(&frames)),
            remote: Stream::spawn(call_id.clone(), VideoStream::Remote, frames),
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
    /// Both directions, because the channel that lost them carries both and
    /// a gap in it says nothing about which was in flight. Each decoder then
    /// waits for a point it can start from rather than rendering frames built
    /// on references it never received.
    pub fn interrupted(&self) {
        self.local.interrupted();
        self.remote.interrupted();
    }

    /// Hand one access unit to the decoder that owns its direction.
    ///
    /// Dropped rather than queued when that decoder is busy: the next unit is
    /// a better picture than this one, and the sender will produce a keyframe
    /// once it learns something was lost.
    pub fn accept(&self, frame: CallVideoFrame) {
        match frame.stream {
            VideoStream::Local => self.local.accept(frame),
            VideoStream::Remote => self.remote.accept(frame),
        }
    }
}

/// How many access units may wait for a decoder.
///
/// Deep enough that an ordinary hitch — a frame the window spent long on, a
/// scheduler that looked elsewhere — costs nothing, and shallow enough that
/// what waits here is never old enough to be worth less than the next one.
const QUEUE_DEPTH: usize = 4;

/// One direction: a thread, its decoder, and the short queue in front.
struct Stream {
    units: std::sync::mpsc::SyncSender<CallVideoFrame>,
    /// Set when something was lost, and spent on the next unit that gets
    /// through — which is the one the loss is *about*. Touched only from the
    /// sending side, so the queue's order is the gap's order.
    gap: AtomicBool,
}

impl Stream {
    fn spawn(call_id: String, stream: VideoStream, frames: FrameSink) -> Self {
        let (units, queue) = std::sync::mpsc::sync_channel::<CallVideoFrame>(QUEUE_DEPTH);
        let name = match stream {
            VideoStream::Local => "oxidezap-selfview",
            VideoStream::Remote => "oxidezap-callvideo",
        };
        // A thread that cannot be spawned leaves the queue's receiver dropped,
        // which makes every `accept` a no-op: the call runs without a picture
        // rather than not running.
        if let Err(e) = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || decode_loop(&call_id, stream, &queue, &frames))
        {
            log::error!("no thread for the {stream:?} video of a call: {e}");
        }
        Self {
            units,
            gap: AtomicBool::new(false),
        }
    }

    /// Something upstream lost units. The next one through says so.
    fn interrupted(&self) {
        self.gap.store(true, Ordering::Relaxed);
    }

    fn accept(&self, frame: CallVideoFrame) {
        // Taken before the send and restored if it fails, so the mark lands
        // on the first unit that actually reaches the decoder — the one whose
        // references are the ones missing.
        let gap = self.gap.swap(false, Ordering::Relaxed) || frame.gap;
        if self.units.try_send(frame.after_a_gap(gap)).is_err() {
            // What follows this unit references it, and a decoder fed the
            // remainder produces a second of torn picture over the last good
            // one. Waiting for a keyframe instead is a freeze, which is at
            // least honest — and a short one, because the sender emits one
            // every few seconds.
            self.gap.store(true, Ordering::Relaxed);
        }
    }
}

fn decode_loop(
    call_id: &str,
    stream: VideoStream,
    queue: &std::sync::mpsc::Receiver<CallVideoFrame>,
    frames: &FrameSink,
) {
    let mut decoder = match Decoder::new() {
        Ok(decoder) => decoder,
        Err(e) => {
            log::error!("no H.264 decoder for a call's video: {e}");
            return;
        }
    };
    // Nothing before the first keyframe means anything: a decoder started
    // mid-GOP reports an error per unit until one arrives, and the log is the
    // only thing that would come of it.
    let mut started = false;
    let mut scratch = Scratch::default();

    while let Ok(unit) = queue.recv() {
        // Units before this one were lost, so what this decoder holds no
        // longer matches what the sender encoded against.
        if unit.gap {
            started = false;
        }
        if !started {
            if !unit.keyframe {
                continue;
            }
            started = true;
        }
        // Read before the decoder is handed it, because a decoder allocates
        // its reference and output buffers from the parameter set — from
        // numbers the *peer* chose. `Scratch` refuses an oversized picture,
        // but it refuses one that has already been decoded, which is after
        // the allocation the refusal is for. A unit carrying no parameter set
        // declares no new geometry and is left alone.
        if let Some((width, height)) = super::sps::coded_size(&unit.data)
            && (width as usize).saturating_mul(height as usize) > MAX_PIXELS
        {
            log::warn!("refusing a {width}x{height} video stream on call {call_id}");
            // Not a gap to recover from: every unit that follows references
            // this picture, so the stream stays refused until the peer sends
            // a parameter set describing one that fits.
            started = false;
            continue;
        }
        let picture = match decoder.decode(&unit.data) {
            Ok(Some(yuv)) => yuv,
            // The decoder is buffering, which is normal.
            Ok(None) => continue,
            Err(e) => {
                log::debug!("dropping a video unit of call {call_id}: {e}");
                // A reference was lost. Wait for a point that stands on its
                // own rather than compounding the error over the next second.
                started = false;
                continue;
            }
        };
        let Some(image) = scratch.render(&picture, Rotation::to_upright(unit.orientation)) else {
            continue;
        };
        // Whether it is drawn is the window's decision: a stale frame drawn
        // late is worse than the next one drawn on time, so the sink drops
        // rather than waits.
        frames(CallFrame {
            call_id: unit.call_id,
            stream,
            image,
        });
    }
    log::debug!("{stream:?} video of call {call_id} closed");
}

/// The buffers a frame is turned in, kept across frames.
///
/// A 720p picture is 3.5 MiB of RGBA and another 3.5 for the rotation, and
/// allocating both twenty times a second is work with nothing to show for it.
/// Sized on demand, because the picture's size is the peer's business and can
/// change mid-call when they rotate their phone.
#[derive(Default)]
struct Scratch {
    rgba: Vec<u8>,
    size: (usize, usize),
}

/// The largest picture a frame is allowed to be. The dimensions come off a
/// peer's bitstream, so their product is somebody else's number: 4K is far
/// past anything a call offers and still bounds the allocation.
const MAX_PIXELS: usize = 3840 * 2160;

impl Scratch {
    fn render(&mut self, yuv: &DecodedYUV<'_>, rotation: Rotation) -> Option<Arc<RenderImage>> {
        let (width, height) = yuv.dimensions();
        if width == 0 || height == 0 || width.saturating_mul(height) > MAX_PIXELS {
            log::warn!("refusing a {width}x{height} video frame");
            return None;
        }
        // The buffer the image will own. Allocated per frame because that is
        // what `RgbaImage::from_raw` takes ownership of; what used to be
        // allocated per frame *beside* it is the scratch, and an unturned
        // frame does not need one — it is written here and corrected in
        // place. At 720p that is 3.5 MiB a frame, per direction, thirty times
        // a second.
        let mut turned = vec![0; width * height * 4];
        if rotation == Rotation::None {
            yuv.write_rgba8(&mut turned);
            swap_rb_in_place(&mut turned);
        } else {
            if self.size != (width, height) {
                self.rgba = vec![0; width * height * 4];
                self.size = (width, height);
            }
            yuv.write_rgba8(&mut self.rgba);
            // `RenderImage` reads BGRA, and the peer's device orientation is a
            // rotation only they know about.
            write_bgra_rotated(&self.rgba, width, height, rotation, &mut turned);
        }
        let (drawn_width, drawn_height) = if rotation.transposes() {
            (height, width)
        } else {
            (width, height)
        };
        let image = RgbaImage::from_raw(drawn_width as u32, drawn_height as u32, turned)?;
        Some(Arc::new(RenderImage::new(SmallVec::from_elem(
            Frame::new(image),
            1,
        ))))
    }
}
