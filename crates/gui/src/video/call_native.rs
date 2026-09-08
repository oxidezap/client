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

use super::geometry::{Rotation, swap_rb_in_place, write_bgra_rotated};

use super::recovery::h264::MAX_PIXELS;
use super::recovery::{Recovery, RecoverySink};

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
        let mut slots = self.lock();
        let slot = slot_of(frame.stream);
        slots[slot] = Some(frame);
    }

    /// Everything waiting, in one pass, leaving the slots empty.
    #[cfg(test)]
    pub fn take(&self) -> SmallVec<[CallFrame; 2]> {
        self.lock().iter_mut().filter_map(Option::take).collect()
    }

    /// Leave pictures whose enabling state has not reached the UI in their slots.
    pub fn take_for(&self, calls: &oxidezap_core::CallState) -> SmallVec<[CallFrame; 2]> {
        let Some(call) = calls.active() else {
            return SmallVec::new();
        };
        self.lock()
            .iter_mut()
            .filter_map(|slot| {
                if slot.as_ref().is_some_and(|frame| {
                    frame.call_id == call.call_id && call.video.is_on(frame.stream)
                }) {
                    slot.take()
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn clear(&self, stream: VideoStream) {
        self.lock()[slot_of(stream)] = None;
    }

    /// Poisoned or not. `put` runs on a decode thread and `take` on the
    /// window's, so panicking here turns a panic in one decoder into a panic
    /// in the UI on its next read: the call and the window go down together.
    /// What is behind the lock is two `Option`s with no invariant to break.
    fn lock(&self) -> std::sync::MutexGuard<'_, [Option<CallFrame>; 2]> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One decoded picture, and which side of the call it is.
pub struct CallFrame {
    pub call_id: String,
    pub stream: VideoStream,
    pub image: Arc<RenderImage>,
}

/// One direction of the call being drawn.
///
/// Created on this direction's first frame and dropped when its camera or
/// call ends. The opposite direction keeps its own reference chain.
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
            decoder: Stream::spawn(call_id.clone(), stream, frames, recover),
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
    ///
    /// Dropped rather than queued when that decoder is busy: the next unit is
    /// a better picture than this one, and the sender will produce a keyframe
    /// once it learns something was lost.
    pub fn accept(&self, frame: CallVideoFrame) {
        if frame.call_id != self.call_id || frame.stream != self.stream {
            return;
        }
        self.decoder.accept(frame);
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
    recovery: Arc<Recovery>,
}

impl Stream {
    fn spawn(
        call_id: String,
        stream: VideoStream,
        frames: FrameSink,
        recover: RecoverySink,
    ) -> Self {
        let (units, queue) = std::sync::mpsc::sync_channel::<CallVideoFrame>(QUEUE_DEPTH);
        let recovery = Arc::new(Recovery::new(call_id.clone(), stream, recover));
        let decoding_recovery = recovery.clone();
        let name = match stream {
            VideoStream::Local => "oxidezap-selfview",
            VideoStream::Remote => "oxidezap-callvideo",
        };
        // A thread that cannot be spawned leaves the queue's receiver dropped,
        // which makes every `accept` a no-op: the call runs without a picture
        // rather than not running.
        if let Err(e) = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || decode_loop(&call_id, stream, &queue, &frames, &decoding_recovery))
        {
            log::error!("no thread for the {stream:?} video of a call: {e}");
        }
        Self {
            units,
            gap: AtomicBool::new(false),
            recovery,
        }
    }

    /// Something upstream lost units. The next one through says so.
    fn interrupted(&self) {
        self.gap.store(true, Ordering::Relaxed);
        self.recovery.request();
    }

    fn accept(&self, frame: CallVideoFrame) {
        // Taken before the send and restored if it fails, so the mark lands
        // on the first unit that actually reaches the decoder — the one whose
        // references are the ones missing.
        let gap = self.gap.swap(false, Ordering::Relaxed) || frame.gap;
        if let Err(std::sync::mpsc::TrySendError::Full(_)) =
            self.units.try_send(frame.after_a_gap(gap))
        {
            // What follows this unit references it, and a decoder fed the
            // remainder produces a second of torn picture over the last good
            // one. Waiting for a keyframe instead is a freeze, which is at
            // least honest. Request a recovery point rather than relying on
            // the sender to emit periodic keyframes.
            self.gap.store(true, Ordering::Relaxed);
            self.recovery.request();
        }
    }
}

fn decode_loop(
    call_id: &str,
    stream: VideoStream,
    queue: &std::sync::mpsc::Receiver<CallVideoFrame>,
    frames: &FrameSink,
    recovery: &Recovery,
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
    let mut access_units = super::recovery::h264::AccessUnits::default();

    while let Ok(unit) = queue.recv() {
        // Units before this one were lost, so what this decoder holds no
        // longer matches what the sender encoded against.
        if unit.gap {
            started = false;
        }
        let (data, recovers) = match access_units.prepare(&unit.data) {
            Ok(Some(picture)) => picture,
            Ok(None) => {
                if !started {
                    recovery.request();
                }
                continue;
            }
            Err(_) => {
                started = false;
                recovery.request();
                continue;
            }
        };
        if !started {
            if !recovers {
                recovery.request();
                continue;
            }
            started = true;
            recovery.admitted();
        }
        let picture = match decoder.decode(&data) {
            Ok(Some(yuv)) => yuv,
            // The decoder is buffering, which is normal.
            Ok(None) => continue,
            Err(e) => {
                log::debug!("dropping a video unit of call {call_id}: {e}");
                // A reference was lost. Wait for a point that stands on its
                // own rather than compounding the error over the next second.
                started = false;
                recovery.request();
                continue;
            }
        };
        let Some(image) = scratch.render(&picture, Rotation::to_upright(unit.orientation)) else {
            continue;
        };
        recovery.output();
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

#[cfg(test)]
mod tests {
    use super::*;

    mod fixture {
        include!("../../tests/webcodecs/h264_fixture.rs");
    }

    fn encoded_pair() -> (Vec<u8>, Vec<u8>) {
        use openh264::encoder::{Encoder, EncoderConfig};
        use openh264::formats::{RgbSliceU8, YUVBuffer};
        let mut encoder =
            Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new())
                .unwrap();
        let pixels = vec![80; 32 * 32 * 3];
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&pixels, (32, 32)));
        (
            encoder.encode(&yuv).unwrap().to_vec(),
            encoder.encode(&yuv).unwrap().to_vec(),
        )
    }

    fn call_outputs(units: Vec<Vec<u8>>) -> (usize, usize) {
        let (sender, queue) = std::sync::mpsc::channel();
        for data in units {
            sender
                .send(CallVideoFrame::new(
                    "generated".into(),
                    VideoStream::Remote,
                    data,
                    true,
                    0,
                ))
                .unwrap();
        }
        drop(sender);
        let outputs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = outputs.clone();
        let frames: FrameSink = Arc::new(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
        });
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = requests.clone();
        let recovery = Recovery::new(
            "generated".into(),
            VideoStream::Remote,
            Arc::new(move |_, _| {
                count.fetch_add(1, Ordering::Relaxed);
                true
            }),
        );
        decode_loop("generated", VideoStream::Remote, &queue, &frames, &recovery);
        (
            outputs.load(Ordering::Relaxed),
            requests.load(Ordering::Relaxed),
        )
    }

    #[test]
    fn repeated_parameter_sets_preserve_the_active_reference_chain() {
        use super::super::recovery::h264::split_annexb;
        let (idr, delta) = encoded_pair();
        let mut sets = Vec::new();
        for nal in split_annexb(&idr).filter(|nal| matches!(nal[0] & 31, 7 | 8)) {
            sets.extend_from_slice(&[0, 0, 0, 1]);
            sets.extend_from_slice(nal);
        }
        let mut direct = Decoder::new().unwrap();
        assert!(direct.decode(&idr).unwrap().is_some());
        assert!(direct.decode(&sets).unwrap().is_none());
        assert!(direct.decode(&delta).unwrap().is_some());
        assert_eq!(call_outputs(vec![idr, sets.clone(), sets, delta]), (2, 0));
    }

    #[test]
    fn an_idr_preserves_pps_announced_for_a_later_delta() {
        use super::super::recovery::h264::split_annexb;
        let (idr, delta) = encoded_pair();
        let mut announced = Vec::new();
        let mut separate = Vec::new();
        for nal in split_annexb(&idr) {
            if nal[0] & 31 == 7 {
                assert_eq!(nal[1], 66, "CAVLC baseline fixture");
            }
            announced.extend_from_slice(&[0, 0, 0, 1]);
            announced.extend_from_slice(nal);
            if nal[0] & 31 == 8 {
                separate.extend_from_slice(&[0, 0, 0, 1]);
                separate.extend(fixture::with_pps_id(nal, 1));
                announced.extend_from_slice(&separate);
            }
        }
        let mut changed_delta = Vec::new();
        for nal in split_annexb(&delta) {
            changed_delta.extend_from_slice(&[0, 0, 0, 1]);
            if nal[0] & 31 == 1 {
                changed_delta.extend(fixture::with_pps_id(nal, 1));
            } else {
                changed_delta.extend_from_slice(nal);
            }
        }
        for units in [
            vec![announced, changed_delta.clone()],
            vec![idr, separate, changed_delta],
        ] {
            let mut direct = Decoder::new().unwrap();
            let direct_count = units
                .iter()
                .filter(|data| direct.decode(data).unwrap().is_some())
                .count();
            assert_eq!(
                direct_count, 2,
                "edited stream must decode without preparation"
            );
            assert_eq!(call_outputs(units), (direct_count, 0));
        }
    }

    #[test]
    fn unflagged_idr_starts_and_recovers_real_decoder() {
        use openh264::encoder::{Encoder, EncoderConfig};
        use openh264::formats::{RgbSliceU8, YUVBuffer};
        let mut encoder =
            Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new())
                .unwrap();
        let pixels = vec![80; 32 * 32 * 3];
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&pixels, (32, 32)));
        let idr = encoder.encode(&yuv).unwrap().to_vec();
        let delta = encoder.encode(&yuv).unwrap().to_vec();
        let mut sets = Vec::new();
        for nal in super::super::recovery::h264::split_annexb(&idr)
            .filter(|nal| matches!(nal[0] & 31, 7 | 8))
        {
            sets.extend_from_slice(&[0, 0, 0, 1]);
            sets.extend_from_slice(nal);
        }
        let (sender, queue) = std::sync::mpsc::channel();
        for gap in [false, true] {
            for data in [&sets, &delta] {
                sender
                    .send(
                        CallVideoFrame::new(
                            "generated-call".into(),
                            VideoStream::Remote,
                            data.clone(),
                            true,
                            0,
                        )
                        .after_a_gap(gap),
                    )
                    .unwrap();
            }
            sender
                .send(
                    CallVideoFrame::new(
                        "generated-call".into(),
                        VideoStream::Remote,
                        idr.clone(),
                        false,
                        0,
                    )
                    .after_a_gap(gap),
                )
                .unwrap();
        }
        drop(sender);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let outputs = count.clone();
        let frames: FrameSink = Arc::new(move |_| {
            outputs.fetch_add(1, Ordering::Relaxed);
        });
        let recovery = Recovery::new(
            "generated-call".into(),
            VideoStream::Remote,
            Arc::new(|_, _| true),
        );
        decode_loop(
            "generated-call",
            VideoStream::Remote,
            &queue,
            &frames,
            &recovery,
        );
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn decoder_failure_requests_the_failed_direction() {
        let data = vec![0, 0, 0, 1, 0x65, 0xff];
        assert!(Decoder::new().unwrap().decode(&data).is_err());
        let (sender, queue) = std::sync::mpsc::channel();
        sender
            .send(CallVideoFrame::new(
                "failed-call".into(),
                VideoStream::Remote,
                data,
                true,
                0,
            ))
            .unwrap();
        drop(sender);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = requests.clone();
        let recovery = Recovery::new(
            "failed-call".into(),
            VideoStream::Remote,
            Arc::new(move |id, stream| {
                received.lock().unwrap().push((id.to_owned(), stream));
                true
            }),
        );
        let frames: FrameSink = Arc::new(|_| panic!("invalid unit produced a picture"));
        decode_loop(
            "failed-call",
            VideoStream::Remote,
            &queue,
            &frames,
            &recovery,
        );
        assert_eq!(
            *requests.lock().unwrap(),
            vec![("failed-call".into(), VideoStream::Remote)]
        );
    }

    #[test]
    fn decoder_overflow_requests_recovery_and_preserves_the_gap_position() {
        let (units, queue) = std::sync::mpsc::sync_channel(1);
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let received = requests.clone();
        let recovery = Arc::new(Recovery::new(
            "test-call".into(),
            VideoStream::Remote,
            Arc::new(move |id, stream| {
                received.lock().unwrap().push((id.to_owned(), stream));
                true
            }),
        ));
        let stream = Stream {
            units,
            gap: AtomicBool::new(false),
            recovery,
        };
        let unit =
            || CallVideoFrame::new("test-call".into(), VideoStream::Remote, vec![], false, 0);
        stream.accept(unit());
        for _ in 0..20 {
            stream.accept(unit());
        }
        assert_eq!(
            *requests.lock().unwrap(),
            vec![("test-call".into(), VideoStream::Remote)]
        );
        assert!(!queue.recv().unwrap().gap);
        stream.accept(unit());
        assert!(queue.recv().unwrap().gap);
    }

    /// `put` runs on a decode thread and `take` on the window's. Panicking on
    /// a poisoned lock turned a panic in one decoder into a panic in the UI
    /// on its next read, so the call and the window went down together.
    /// over two `Option`s with no invariant to break.
    #[test]
    fn a_panicked_decoder_does_not_take_the_window_with_it() {
        let frames = LatestFrames::default();
        let poisoner = frames.clone();
        let panicked = std::thread::spawn(move || {
            let _held = poisoner.lock();
            panic!("a decoder gave up mid-frame");
        })
        .join();
        assert!(panicked.is_err(), "the lock is poisoned now");

        assert!(frames.take().is_empty(), "and the window can still read it");
    }
}
