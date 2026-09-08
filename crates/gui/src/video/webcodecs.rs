//! H.264, decoded by the browser.
//!
//! The desktop links openh264, which is C and has no toolchain behind
//! `wasm32-unknown-unknown`. A browser has the same decoder in hardware and
//! hands it over as WebCodecs, so what is missing on this target is the
//! binding rather than the capability.
//!
//! # What this is not
//!
//! It is not a `Decoder` with a different name. openh264 is *pulled*: hand it
//! an access unit, get a picture back on the same line. `VideoDecoder` is
//! pushed — units go in, pictures arrive on a callback later, and reading the
//! pixels out of one is itself asynchronous. Nothing above can be handed a
//! frame synchronously any more, so what this offers instead is a slot: feed
//! it, and read whatever has landed when you next draw.
//!
//! That shape suits both callers. A conversation's video is drawn on a timer
//! that is already asking every frame, and a call is a stream where the
//! newest picture is the only one worth having.
//!
//! # Failing back
//!
//! Every entry point answers `None` or an error rather than panicking, and a
//! caller that gets one is expected to behave exactly as this platform did
//! before WebCodecs was bound at all: say the video cannot be played here.
//! A browser without WebCodecs, a codec it will not configure, a picture past
//! the budget — all of them land there, which is why none of them is fatal.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gpui::RenderImage;
use image::{Frame, RgbaImage};
use smallvec::SmallVec;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

use super::geometry::{
    MAX_VIDEO_PIXELS, Rotation, TurnLog, declares_more_than, declares_unreadably, frame_byte_len,
    into_bgra_transformed,
};

/// The newest decoded picture, and what has gone wrong.
///
/// Shared between the decoder and whoever draws, because the pictures arrive
/// on the browser's own callback rather than on any call this side makes.
#[derive(Default)]
struct Slot {
    /// The newest picture, overwriting whatever it found.
    ///
    /// A slot rather than a queue for the reason `LatestFrames` is one: a
    /// picture that could not be drawn when it arrived is worth nothing once
    /// the next has come, and a queue of them is a tab's memory spent on
    /// video nobody saw.
    newest: Option<Picture>,
    /// Set once the decoder has refused something. Sticky: a decoder that has
    /// errored produces nothing further, so the first reason is the useful
    /// one and later ones are consequences.
    failed: Option<String>,
    /// The sequence number of the newest picture that was accepted.
    ///
    /// Reading the pixels out of a frame is asynchronous, so several copies
    /// can be outstanding at once and they may resolve in any order. Without
    /// this the last one to *finish* wins rather than the last one to be
    /// decoded, which moves an attachment backwards a frame and puts a stale
    /// picture on a call pane.
    accepted: u64,
}

/// One decoded picture, in the form gpui draws.
#[derive(Clone)]
pub struct Picture {
    pub image: Arc<RenderImage>,
    /// The presentation timestamp the chunk carried, in microseconds.
    pub timestamp_micros: i64,
}

/// A `VideoDecoder`, its callbacks, and the slot they write into.
pub struct Decoder {
    inner: web_sys::VideoDecoder,
    readback: Rc<Readback>,
    slot: Rc<RefCell<Slot>>,
    /// The `avc1` string the decoder was configured with.
    ///
    /// Kept because `reset` has to configure it again: `VideoDecoder::reset`
    /// leaves the decoder *unconfigured* by the WebCodecs specification, so a
    /// reset that only cleared the slot left every later `decode` refused,
    /// and the first refusal is sticky, so the picture never came back.
    codec: String,
    /// Which decoder generation the pictures now arriving belong to.
    ///
    /// Bumped by `reset`, and read by each copy as it completes: a copy
    /// started before a seek resolves after it, and the picture it carries is
    /// from a position nobody is looking at any more.
    generation: Rc<Cell<u64>>,
    /// How many frames have been handed to a copy, which is the order they
    /// were decoded in. See [`Slot::accepted`].
    submitted: Rc<Cell<u64>>,
    /// Set when a picture was dropped because too many copies were in
    /// flight.
    ///
    /// The drop is right for a call, where a frame nobody could read is a
    /// frame worth losing. It is wrong for an attachment, where the feed has
    /// already advanced past the target it asked for and would otherwise sit
    /// on an older picture for ever, waiting for one that was thrown away.
    /// So the fact is reported rather than only acted on, and the caller that
    /// cares asks.
    refused: Rc<Cell<bool>>,
    /// Live calls keep one active readback and replace only the pending output.
    pending: Rc<RefCell<Option<PendingFrame>>>,
    /// The turn to apply to the next unit fed.
    ///
    /// A cell because a call's is per frame: a peer's orientation describes
    /// their device, and they may turn it mid-call.
    rotation: Rc<Cell<Rotation>>,
    /// The turn each unit still in the decoder was fed under. See
    /// [`TurnLog`], which is where the reasoning and the tests are.
    turns: Rc<RefCell<TurnLog>>,
    /// How many pixels a picture may be before it is refused.
    max_pixels: usize,
    /// Kept alive for as long as the decoder is: a `Closure` that has been
    /// dropped while the browser still holds a reference to it is a call into
    /// freed memory, which on this target is a panic that takes the tab.
    _on_frame: Closure<dyn FnMut(web_sys::VideoFrame)>,
    _on_error: Closure<dyn FnMut(web_sys::DomException)>,
}

impl Decoder {
    /// Build one for a stream whose parameter sets are `sps_pps`.
    ///
    /// Annex B rather than AVCC, because that is what both callers already
    /// have: the container path converts on the way out of `mp4`, and a call
    /// carries access units that way on the wire. A configuration with no
    /// `description` is Annex B by the WebCodecs specification, so the two
    /// need no second shape.
    ///
    /// # Errors
    ///
    /// No `VideoDecoder` in this browser, a parameter set this build will not
    /// read, or a picture past the budget.
    pub fn new(sps_pps: &[u8], rotation: Rotation) -> Result<Self, String> {
        Self::with_budget(sps_pps, rotation, MAX_VIDEO_PIXELS, None)
    }

    /// The same, under a caller's own pixel budget and picture sink.
    ///
    /// A call is tighter than an attachment — 4K is already far past what a
    /// call offers — and it wants each picture as it lands rather than the
    /// newest when it next draws, because the window's own frame slot is
    /// where a call's pictures are held.
    pub fn with_budget(
        sps_pps: &[u8],
        rotation: Rotation,
        max_pixels: usize,
        sink: Option<Rc<dyn Fn(Picture)>>,
    ) -> Result<Self, String> {
        // Before anything is configured, for the reason the native decoder
        // asks before it allocates: the numbers come from a file somebody
        // sent, and a budget applied after the decoder has sized its own
        // buffers is applied after the allocation it exists to prevent.
        if let Some((width, height)) = declares_more_than(sps_pps, max_pixels) {
            return Err(format!("refusing a {width}x{height} video stream"));
        }
        // A budget nothing can apply is not a budget: a parameter set the
        // parser gives up on is a picture the decoder is about to allocate
        // from, unchecked, and its shape is whoever produced the file's to
        // choose.
        if declares_unreadably(sps_pps) {
            return Err("refusing a video stream whose geometry cannot be read".to_string());
        }
        let codec = codec_string(sps_pps)
            .ok_or_else(|| "no readable parameter set in this stream".to_string())?;

        let slot = Rc::new(RefCell::new(Slot::default()));
        let rotation = Rc::new(Cell::new(rotation));

        let generation = Rc::new(Cell::new(0u64));
        let submitted = Rc::new(Cell::new(0u64));
        let in_flight = Rc::new(Cell::new(0usize));
        let refused = Rc::new(Cell::new(false));
        let pending = Rc::new(RefCell::new(None::<PendingFrame>));
        let readback = Rc::new(Readback::default());
        let turns: Rc<RefCell<TurnLog>> = Rc::new(RefCell::new(TurnLog::default()));

        let on_frame = {
            let slot = Rc::clone(&slot);
            let rotation = Rc::clone(&rotation);
            let turns = Rc::clone(&turns);
            let generation = Rc::clone(&generation);
            let submitted = Rc::clone(&submitted);
            let in_flight = Rc::clone(&in_flight);
            let refused = Rc::clone(&refused);
            let pending = Rc::clone(&pending);
            let readback = Rc::clone(&readback);
            Closure::<dyn FnMut(web_sys::VideoFrame)>::new(move |frame: web_sys::VideoFrame| {
                if readback.stats.get().is_some() {
                    let (width, height) = frame.visible_rect().map_or_else(
                        || (frame.coded_width() as usize, frame.coded_height() as usize),
                        |rect| (rect.width() as usize, rect.height() as usize),
                    );
                    readback.count(|s| {
                        s.decoded_outputs = s.decoded_outputs.saturating_add(1);
                        s.latest_dimensions = Some((width, height));
                        let degrees = frame.rotation();
                        let transform =
                            (degrees.is_finite().then_some(degrees as u16), frame.flip());
                        s.latest_display_dimensions =
                            Some((frame.display_width(), frame.display_height()));
                        if degrees.is_finite() && (degrees != 0.0 || frame.flip()) {
                            s.transformed_outputs = s.transformed_outputs.saturating_add(1);
                        }
                        if s.latest_display_transform != Some(transform) {
                            s.display_transform_changes =
                                s.display_transform_changes.saturating_add(1);
                            s.latest_display_transform = Some(transform);
                        }
                        s.min_dimensions = Some(
                            s.min_dimensions
                                .map_or((width, height), |(w, h)| (w.min(width), h.min(height))),
                        );
                        s.max_dimensions = Some(
                            s.max_dimensions
                                .map_or((width, height), |(w, h)| (w.max(width), h.max(height))),
                        );
                    });
                    readback.report(false);
                }
                let turn = turns
                    .borrow_mut()
                    .take(frame.timestamp() as i32)
                    .unwrap_or_else(|| rotation.get());
                // Dropped rather than queued, which is what every queue on
                // this path does: the slot holds one picture, so a frame
                // arriving while that many copies are still outstanding is
                // one nobody was going to see. Closing it is not optional
                // either, since an unclosed `VideoFrame` pins a decoder
                // buffer and a decoder that runs out stops producing.
                if sink.is_none() && in_flight.get() >= MAX_COPIES_IN_FLIGHT {
                    readback.count(|s| s.dropped_pending = s.dropped_pending.saturating_add(1));
                    frame.close();
                    refused.set(true);
                    return;
                }
                let seq = submitted.get().wrapping_add(1);
                submitted.set(seq);
                let work = PendingFrame {
                    frame,
                    rotation: turn,
                    stamp: Stamp {
                        generation: Rc::clone(&generation),
                        born: generation.get(),
                        seq,
                    },
                };
                // Only decoded output is replaced. Compressed units must all
                // reach the decoder to preserve its reference chain.
                if sink.is_some() && in_flight.get() != 0 {
                    if pending.borrow_mut().replace(work).is_some() {
                        readback.count(|s| s.dropped_pending = s.dropped_pending.saturating_add(1));
                    }
                    return;
                }
                let outstanding = Outstanding::new(Rc::clone(&in_flight));
                let pending = Rc::clone(&pending);
                let readback = Rc::clone(&readback);
                let slot = Rc::clone(&slot);
                let sink = sink.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let _outstanding = outstanding;
                    let mut work = Some(work);
                    while let Some(frame) = work {
                        read_frame(frame, max_pixels, &slot, sink.as_ref(), &readback).await;
                        work = pending.borrow_mut().take();
                    }
                });
            })
        };
        let on_error = {
            let slot = Rc::clone(&slot);
            Closure::<dyn FnMut(web_sys::DomException)>::new(move |e: web_sys::DomException| {
                let mut slot = slot.borrow_mut();
                if slot.failed.is_none() {
                    slot.failed = Some(format!("the browser's decoder stopped: {}", e.message()));
                }
            })
        };

        let init = web_sys::VideoDecoderInit::new(
            on_error.as_ref().unchecked_ref(),
            on_frame.as_ref().unchecked_ref(),
        );
        let inner = web_sys::VideoDecoder::new(&init)
            .map_err(|e| format!("this browser has no video decoder: {e:?}"))?;

        // Install the close guard before configure can fail.
        let decoder = Self {
            inner,
            readback,
            slot,
            codec,
            generation,
            submitted,
            pending,
            refused,
            rotation,
            turns,
            max_pixels,
            _on_frame: on_frame,
            _on_error: on_error,
        };
        let config = web_sys::VideoDecoderConfig::new(&decoder.codec);
        // Left to the browser: it knows its own hardware, and the picture is
        // read back through `copy_to` either way.
        decoder
            .inner
            .configure(&config)
            .map_err(|e| format!("the browser would not decode {}: {e:?}", decoder.codec))?;

        Ok(decoder)
    }

    /// Hand one access unit to the decoder.
    ///
    /// `is_key` decides the chunk's type, and the browser refuses a delta
    /// chunk before it has seen a key one — which is the same rule the rest
    /// of this tree already obeys, since every drop here asks for an IDR.
    pub fn decode(&self, access_unit: &[u8], timestamp_micros: i32, is_key: bool) {
        if self.slot.borrow().failed.is_some() {
            return;
        }
        // Refused per unit rather than once at configuration: a stream may
        // carry a new parameter set at any point, and the browser would size
        // its buffers from whichever it saw last.
        if let Some((width, height)) = declares_more_than(access_unit, self.max_pixels) {
            self.slot.borrow_mut().failed =
                Some(format!("refusing a {width}x{height} video stream"));
            return;
        }
        if declares_unreadably(access_unit) {
            self.slot.borrow_mut().failed =
                Some("refusing a video stream whose geometry cannot be read".to_string());
            return;
        }
        let data = js_sys::Uint8Array::from(access_unit);
        let kind = if is_key {
            web_sys::EncodedVideoChunkType::Key
        } else {
            web_sys::EncodedVideoChunkType::Delta
        };
        let init = web_sys::EncodedVideoChunkInit::new_with_u8_array(&data, timestamp_micros, kind);
        let Ok(chunk) = web_sys::EncodedVideoChunk::new(&init) else {
            return;
        };
        // Stamped with the turn it goes in under, so the picture that comes
        // back can be drawn the way it was encoded rather than the way the
        // peer is holding their device by then. Bounded by the same depth the
        // callers bound the decode queue at, twice over, so a stamp whose
        // picture never arrives cannot accumulate.
        self.turns
            .borrow_mut()
            .record(timestamp_micros, self.rotation.get());
        if let Err(e) = self.inner.decode(&chunk) {
            let mut slot = self.slot.borrow_mut();
            if slot.failed.is_none() {
                slot.failed = Some(format!("the browser refused a frame: {e:?}"));
            }
        }
    }

    /// The newest picture, if one has arrived.
    ///
    /// Cloned rather than taken: a caller drawing every frame would otherwise
    /// blank the picture between arrivals, which for a paused video is the
    /// picture disappearing while it is being looked at.
    pub fn newest(&self) -> Option<Picture> {
        self.slot.borrow().newest.clone()
    }

    /// Why the decoder stopped, if it has.
    pub fn failure(&self) -> Option<String> {
        self.slot.borrow().failed.clone()
    }

    /// Enable once, before feeding any units. The callback receives cumulative
    /// snapshots on output activity and a final, frozen snapshot on drop.
    /// The caller owns log throttling; this module reads no clock.
    pub fn enable_diagnostics(&self, report: impl Fn(Diagnostics, bool) + 'static) {
        if self.diagnostics().is_none() && self.submitted.get() == 0 {
            self.readback.stats.set(Some(Diagnostics::default()));
            *self.readback.reporter.borrow_mut() = Some(Rc::new(report));
        }
    }

    pub fn diagnostics(&self) -> Option<Diagnostics> {
        self.readback.stats.get()
    }

    /// Whether a picture was dropped for want of a copy slot since this was
    /// last asked, and clear the fact.
    ///
    /// A caller that walks to a target has to know: the unit was fed, so its
    /// index has advanced, and the picture that would have answered is gone.
    /// Without asking, the walk waits for something that is never coming.
    pub fn take_refusal(&self) -> bool {
        self.refused.replace(false)
    }

    /// Forget everything decoded so far and start again at a keyframe.
    ///
    /// What a seek costs on this path: the browser's decoder has its own
    /// reference chain, and feeding it units from the middle of one produces
    /// nothing until the next IDR.
    pub fn reset(&self) {
        // Before anything else, so a copy already in flight is recognised as
        // belonging to the stream that has just been left behind.
        self.generation.set(self.generation.get().wrapping_add(1));
        if self.pending.borrow_mut().take().is_some() {
            self.readback
                .count(|s| s.dropped_obsolete = s.dropped_obsolete.saturating_add(1));
        }
        self.submitted.set(0);
        self.refused.set(false);

        let _ = self.inner.reset();
        self.turns.borrow_mut().clear();
        {
            let mut slot = self.slot.borrow_mut();
            slot.newest = None;
            slot.accepted = 0;
        }

        // `reset` returns the decoder to `unconfigured`, so this is not
        // housekeeping: without it the next `decode` is refused, the refusal
        // is sticky, and every frame after a seek is dropped before it
        // reaches the browser.
        let config = web_sys::VideoDecoderConfig::new(&self.codec);
        let mut slot = self.slot.borrow_mut();
        match self.inner.configure(&config) {
            // Cleared only here: a decoder that has been configured again is
            // one whose earlier failure has nothing left to say.
            Ok(()) => slot.failed = None,
            Err(e) => {
                slot.failed = Some(format!(
                    "the browser would not decode {} again: {e:?}",
                    self.codec
                ));
            }
        }
    }

    /// How many units the browser has taken and not yet decoded.
    ///
    /// The desktop path bounds its own queue at `QUEUE_DEPTH`; here the queue
    /// is the browser's and the only thing this side can do about it is stop
    /// feeding. A caller that outruns the decoder banks compressed units for
    /// the length of a call, and draws a picture further behind with each one.
    pub fn queued(&self) -> u32 {
        self.inner.decode_queue_size()
    }

    /// Turn the next pictures a different way.
    ///
    /// A call's orientation travels on each frame, so this is set before the
    /// unit that carries it is fed.
    pub fn set_rotation(&self, rotation: Rotation) {
        self.rotation.set(rotation);
    }
}

impl Drop for Decoder {
    /// Close the decoder rather than leaving it to the collector.
    ///
    /// It holds a hardware decode session, and a tab that opens one per video
    /// in a conversation runs out of them long before it runs out of memory.
    fn drop(&mut self) {
        // A dropped decoder's copies are still outstanding, and its slot and
        // its sink outlive it: a call replaces the decoder without replacing
        // either, so a picture from the old one would land on the new stream.
        self.generation.set(self.generation.get().wrapping_add(1));
        self.pending.borrow_mut().take();
        let _ = self.inner.close();
        // Include pending and active work now, then freeze before late promises
        // settle. Every output not already terminal becomes obsolete on drop.
        self.readback.count(|s| {
            s.dropped_obsolete = s
                .decoded_outputs
                .saturating_sub(s.dropped_pending)
                .saturating_sub(s.materialized)
                .saturating_sub(s.failed_frames);
        });
        self.readback.report(true);
    }
}

/// Attachments retain concurrent copies and report overflow for seek replay.
const MAX_COPIES_IN_FLIGHT: usize = 8;

/// One outstanding pixel copy, counted while it lives.
///
/// A guard rather than a decrement at the end of the task, because the copy
/// has several ways to finish and only one of them is the ordinary one.
struct Outstanding(Rc<Cell<usize>>);

impl Outstanding {
    fn new(count: Rc<Cell<usize>>) -> Self {
        count.set(count.get().saturating_add(1));
        Self(count)
    }
}

impl Drop for Outstanding {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

/// Which decoder generation a copy belongs to, and where it sits in it.
///
/// Carried into the asynchronous read so a picture can say whether it is
/// still wanted by the time it is ready. See [`Slot::accepted`] and
/// [`Decoder::generation`].
struct Stamp {
    generation: Rc<Cell<u64>>,
    born: u64,
    seq: u64,
}

impl Stamp {
    /// Whether the decoder that produced this picture is still the one being
    /// drawn from.
    fn current(&self) -> bool {
        self.generation.get() == self.born
    }

    fn wanted(&self, slot: &Slot) -> bool {
        self.current() && self.seq > slot.accepted && slot.failed.is_none()
    }
}

struct PendingFrame {
    frame: web_sys::VideoFrame,
    rotation: Rotation,
    stamp: Stamp,
}

impl Drop for PendingFrame {
    fn drop(&mut self) {
        self.frame.close();
    }
}

type DiagnosticsReporter = Rc<dyn Fn(Diagnostics, bool)>;

#[derive(Default)]
struct Readback {
    spare: RefCell<Option<js_sys::Uint8Array>>,
    bgra: Cell<Option<bool>>,
    probe: RefCell<Option<js_sys::Promise>>,
    stats: Cell<Option<Diagnostics>>,
    reporter: RefCell<Option<DiagnosticsReporter>>,
}

/// Fixed-size cumulative totals. Readbacks count actual copyTo attempts and
/// settled promises, including rejected attempts and RGBA retries, not the probe.
/// Dimensions are visible output dimensions before rotation, with per-axis extrema.
/// Pending drops include attachment overflow. Obsolete drops include reset,
/// teardown, superseded copies, and queued work discarded after decoder failure.
/// Requested bytes include failed attempts; materialized bytes count published images.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    pub decoded_outputs: u64,
    pub readbacks_started: u64,
    pub readbacks_completed: u64,
    pub materialized: u64,
    pub dropped_pending: u64,
    pub dropped_obsolete: u64,
    pub failed_frames: u64,
    pub bytes_requested: u64,
    pub bytes_materialized: u64,
    pub latest_dimensions: Option<(usize, usize)>,
    /// Actual decoder output rotation in degrees and flip, not RTP input.
    /// An absent rotation means the browser does not expose the property.
    pub latest_display_transform: Option<(Option<u16>, bool)>,
    pub display_transform_changes: u64,
    pub transformed_outputs: u64,
    pub latest_display_dimensions: Option<(u32, u32)>,
    pub min_dimensions: Option<(usize, usize)>,
    pub max_dimensions: Option<(usize, usize)>,
    pub rgba: u64,
    pub bgra: u64,
    pub probe_fallbacks: u64,
    pub allocation_fallbacks: u64,
    pub copy_fallbacks: u64,
}

impl Readback {
    fn count(&self, update: impl FnOnce(&mut Diagnostics)) {
        if let Some(mut stats) = self.stats.get() {
            update(&mut stats);
            self.stats.set(Some(stats));
        }
    }

    fn report(&self, final_report: bool) {
        let stats = if final_report {
            self.stats.take()
        } else {
            self.stats.get()
        };
        let reporter = if final_report {
            self.reporter.borrow_mut().take()
        } else {
            self.reporter.borrow().clone()
        };
        if let (Some(stats), Some(reporter)) = (stats, reporter) {
            reporter(stats, final_report);
        }
    }

    fn started(&self, bgra: bool, bytes: usize) {
        self.count(|s| {
            s.readbacks_started = s.readbacks_started.saturating_add(1);
            s.bytes_requested = s.bytes_requested.saturating_add(bytes as u64);
            if bgra {
                s.bgra = s.bgra.saturating_add(1);
            } else {
                s.rgba = s.rgba.saturating_add(1);
            }
        });
    }

    async fn supports_bgra(&self) -> bool {
        if let Some(supported) = self.bgra.get() {
            return supported;
        }
        let promise = self
            .probe
            .borrow_mut()
            .get_or_insert_with(|| {
                wasm_bindgen_futures::future_to_promise(async {
                    Ok(probe_bgra().await.unwrap_or(false).into())
                })
            })
            .clone();
        let supported = wasm_bindgen_futures::JsFuture::from(promise)
            .await
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        // A concurrent real-frame rejection must not be overwritten by the probe.
        if self.bgra.get().is_none() && !supported {
            self.count(|s| s.probe_fallbacks = s.probe_fallbacks.saturating_add(1));
        }
        self.bgra.set(Some(self.bgra.get().unwrap_or(supported)));
        self.probe.borrow_mut().take();
        self.bgra.get() == Some(true)
    }
}

async fn probe_bgra() -> Result<bool, wasm_bindgen::JsValue> {
    // Unknown dictionary members can be silently ignored. Verify channel order,
    // not merely promise success, once per decoder using four JS-owned bytes.
    let constructor = js_sys::Reflect::get(&js_sys::global(), &"VideoFrame".into())?
        .dyn_into::<js_sys::Function>()?;
    let init = js_sys::Object::new();
    for (key, value) in [
        ("format", "RGBA".into()),
        ("codedWidth", 1.into()),
        ("codedHeight", 1.into()),
        ("timestamp", 0.into()),
    ] {
        js_sys::Reflect::set(&init, &key.into(), &value)?;
    }
    let pixels = js_sys::Uint8Array::from(&[19u8, 73, 151, 255][..]);
    let args = js_sys::Array::new();
    args.push(&pixels);
    args.push(&init);
    let frame: web_sys::VideoFrame =
        js_sys::Reflect::construct(&constructor, &args)?.unchecked_into();
    let options = web_sys::VideoFrameCopyToOptions::new();
    options.set_format(web_sys::VideoPixelFormat::Bgra);
    let destination = js_sys::Uint8Array::new_with_length(4);
    let result = wasm_bindgen_futures::JsFuture::from(
        frame.copy_to_with_buffer_source_and_options(&destination, &options),
    )
    .await;
    frame.close();
    result?;
    Ok(destination.to_vec() == [151, 73, 19, 255])
}

/// Read the pixels out of a decoded frame and put them in the slot.
///
/// Asynchronous, because `copy_to` is: the frame is closed as soon as the
/// copy resolves, since an unclosed `VideoFrame` pins a decoder buffer and a
/// decoder that runs out of them stops producing.
async fn read_frame(
    work: PendingFrame,
    max_pixels: usize,
    slot: &RefCell<Slot>,
    sink: Option<&Rc<dyn Fn(Picture)>>,
    readback: &Readback,
) {
    let PendingFrame {
        frame,
        rotation,
        stamp,
    } = &work;
    if !stamp.wanted(&slot.borrow()) {
        readback.count(|s| s.dropped_obsolete = s.dropped_obsolete.saturating_add(1));
        return;
    }
    // The *visible* rectangle, not the coded one. `copyTo` copies the visible
    // region by default, and a coded frame is padded out to whole macroblocks
    // — 1080 is not a multiple of 16 — so sizing the buffer from
    // `coded_height` and then walking it as if the padding were there lays
    // compact rows out against a wider stride. What that looks like is not a
    // black band at the bottom but every row after the first sliding
    // sideways, which is the kind of wrong that reads as a decoder bug.
    let (width, height) = frame.visible_rect().map_or_else(
        || (frame.coded_width() as usize, frame.coded_height() as usize),
        |rect| (rect.width() as usize, rect.height() as usize),
    );
    let timestamp_micros = frame.timestamp() as i64;
    let flip = frame.flip();
    // copyTo returns untransformed visible pixels; drawImage applies the frame's
    // clockwise rotation, then horizontal flip, before the caller's transform.
    let internal = Rotation::from_quarter_turns((frame.rotation() / 90.0) as u8);
    let rotation = rotation.after_frame(internal, flip);

    // The decoder's own geometry, never the container's. See
    // [`super::geometry::frame_byte_len`] for why that distinction is the one
    // that matters.
    let Some(byte_len) =
        frame_byte_len(width, height).filter(|_| width.saturating_mul(height) <= max_pixels)
    else {
        readback.count(|s| s.failed_frames = s.failed_frames.saturating_add(1));
        if stamp.current() {
            let mut slot = slot.borrow_mut();
            if slot.failed.is_none() {
                slot.failed = Some(format!("refusing a {width}x{height} video frame"));
            }
        }
        return;
    };

    let mut bgra = rotation == Rotation::None && !flip && readback.supports_bgra().await;
    if !stamp.wanted(&slot.borrow()) {
        readback.count(|s| s.dropped_obsolete = s.dropped_obsolete.saturating_add(1));
        return;
    }
    let options = web_sys::VideoFrameCopyToOptions::new();
    options.set_format(if bgra {
        web_sys::VideoPixelFormat::Bgra
    } else {
        web_sys::VideoPixelFormat::Rgba
    });
    let spare = &readback.spare;

    // A JS-owned buffer stays valid until the promise settles, without a
    // borrowed wasm slice crossing an asynchronous binding.
    // Asked of the frame rather than computed, because the browser knows its
    // own layout: `byte_len` above is the budget's arithmetic and this is the
    // buffer the copy will actually fill. They agree for packed RGBA, and
    // where they do not it is the browser that is right.
    let mut allocation = frame.allocation_size_with_options(&options);
    if bgra && allocation.is_err() {
        readback.count(|s| s.allocation_fallbacks = s.allocation_fallbacks.saturating_add(1));
        bgra = false;
        readback.bgra.set(Some(false));
        options.set_format(web_sys::VideoPixelFormat::Rgba);
        allocation = frame.allocation_size_with_options(&options);
    }
    let needed = allocation.map_or(byte_len, |size| size as usize);
    if needed > byte_len {
        readback.count(|s| s.failed_frames = s.failed_frames.saturating_add(1));
        if stamp.current() {
            let mut slot = slot.borrow_mut();
            if slot.failed.is_none() {
                slot.failed = Some(format!(
                    "a decoded frame wanted {needed} bytes, past the budget"
                ));
            }
        }
        return;
    }
    let destination = spare
        .borrow_mut()
        .take()
        .filter(|buffer| buffer.length() as usize == needed)
        .unwrap_or_else(|| js_sys::Uint8Array::new_with_length(needed as u32));
    // Returns the promise directly rather than a `Result`: a `copyTo` that
    // cannot be started rejects rather than throwing, so there is one failure
    // path and it is the awaited one below.
    readback.started(bgra, needed);
    let promise = frame.copy_to_with_buffer_source_and_options(&destination, &options);

    let mut read = wasm_bindgen_futures::JsFuture::from(promise).await;
    readback.count(|s| s.readbacks_completed = s.readbacks_completed.saturating_add(1));
    if bgra && read.is_err() && stamp.wanted(&slot.borrow()) {
        readback.count(|s| s.copy_fallbacks = s.copy_fallbacks.saturating_add(1));
        // A format may work for the probe but not this decoder's backing store.
        // Retry the same open frame before declaring the decoder failed.
        bgra = false;
        readback.bgra.set(Some(false));
        options.set_format(web_sys::VideoPixelFormat::Rgba);
        readback.started(false, needed);
        read = wasm_bindgen_futures::JsFuture::from(
            frame.copy_to_with_buffer_source_and_options(&destination, &options),
        )
        .await;
        readback.count(|s| s.readbacks_completed = s.readbacks_completed.saturating_add(1));
    }
    // Closed on both paths, and before the slot is touched: the buffer it
    // holds is the decoder's, not ours.
    frame.close();
    if let Err(e) = read {
        readback.count(|s| {
            if stamp.wanted(&slot.borrow()) {
                s.failed_frames = s.failed_frames.saturating_add(1);
            } else {
                s.dropped_obsolete = s.dropped_obsolete.saturating_add(1);
            }
        });
        if stamp.wanted(&slot.borrow()) {
            let mut slot = slot.borrow_mut();
            if slot.failed.is_none() {
                slot.failed = Some(format!("could not read a decoded frame: {e:?}"));
            }
        }
        return;
    }
    // The copy resolved, and the decoder it belongs to may have been
    // reset or replaced while it was in flight. Checked before a pixel is
    // laid out, since the work below is only worth doing for a picture
    // somebody is still going to look at.
    if !stamp.wanted(&slot.borrow()) {
        readback.count(|s| s.dropped_obsolete = s.dropped_obsolete.saturating_add(1));
        *spare.borrow_mut() = Some(destination);
        return;
    }

    let source = destination.to_vec();
    *spare.borrow_mut() = Some(destination);
    if source.len() < width * height * 4 {
        readback.count(|s| s.failed_frames = s.failed_frames.saturating_add(1));
        return;
    }
    let bgra = if bgra {
        source
    } else {
        into_bgra_transformed(source, width, height, rotation, flip)
    };
    let (draw_width, draw_height) = if rotation.transposes() {
        (height, width)
    } else {
        (width, height)
    };
    let Some(buffer) = RgbaImage::from_raw(draw_width as u32, draw_height as u32, bgra) else {
        readback.count(|s| s.failed_frames = s.failed_frames.saturating_add(1));
        return;
    };
    let image = Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(buffer), 1)));

    let picture = Picture {
        image,
        timestamp_micros,
    };
    {
        let mut slot = slot.borrow_mut();
        // Copies resolve in whatever order the browser finishes them, so
        // an older picture arriving late is not the newest one: dropped
        // rather than allowed to overwrite what has already been shown.
        if !stamp.wanted(&slot) {
            readback.count(|s| s.dropped_obsolete = s.dropped_obsolete.saturating_add(1));
            return;
        }
        slot.accepted = stamp.seq;
        slot.newest = Some(picture.clone());
    }
    readback.count(|s| {
        s.materialized = s.materialized.saturating_add(1);
        s.bytes_materialized = s.bytes_materialized.saturating_add(needed as u64);
    });
    // After the borrow is released: a sink is the caller's code, and one
    // that asked this decoder anything would find it already borrowed.
    if let Some(sink) = sink {
        sink(picture);
    }
}

/// The `avc1.PPCCLL` string WebCodecs wants, read out of the parameter set.
///
/// The three bytes are the profile, the constraint flags and the level, in
/// the order the sequence parameter set carries them. Read rather than
/// guessed: a fixed `avc1.42E01E` is baseline at level 3, and configuring a
/// high-profile stream as baseline is a decoder that refuses the first frame
/// on some browsers and produces macroblock soup on others.
fn codec_string(sps_pps: &[u8]) -> Option<String> {
    let sps = first_nal_of_type(sps_pps, 7)?;
    // The three bytes follow the one-byte NAL header.
    let profile = *sps.get(1)?;
    let constraints = *sps.get(2)?;
    let level = *sps.get(3)?;
    Some(format!("avc1.{profile:02X}{constraints:02X}{level:02X}"))
}

/// The first NAL unit of a given type in an Annex B stream, header included.
fn first_nal_of_type(stream: &[u8], nal_type: u8) -> Option<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (n, &start) in starts.iter().enumerate() {
        let end = starts
            .get(n + 1)
            .map_or(stream.len(), |&next| next.saturating_sub(3));
        let unit = stream.get(start..end)?;
        // The trailing zero of a four-byte start code belongs to the next
        // unit's prefix rather than to this one's payload.
        let unit = match unit.last() {
            Some(0) if n + 1 < starts.len() => &unit[..unit.len() - 1],
            _ => unit,
        };
        if unit.first().is_some_and(|header| header & 0x1F == nal_type) {
            return Some(unit);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_copies_are_rejected_before_materialization() {
        let generation = Rc::new(Cell::new(4));
        let stamp = Stamp {
            generation: Rc::clone(&generation),
            born: 4,
            seq: 2,
        };
        let mut slot = Slot::default();
        assert!(stamp.wanted(&slot));
        slot.accepted = 2;
        assert!(!stamp.wanted(&slot));
        slot.accepted = 3;
        assert!(!stamp.wanted(&slot));
        slot.accepted = 0;
        generation.set(5);
        assert!(!stamp.wanted(&slot));
    }

    #[test]
    fn failed_decoder_does_not_materialize_a_copy() {
        let stamp = Stamp {
            generation: Rc::new(Cell::new(0)),
            born: 0,
            seq: 1,
        };
        let slot = Slot {
            failed: Some("decoder failed".into()),
            ..Slot::default()
        };
        assert!(!stamp.wanted(&slot));
    }

    #[test]
    fn outstanding_count_is_released_on_drop() {
        let count = Rc::new(Cell::new(0));
        let first = Outstanding::new(Rc::clone(&count));
        let second = Outstanding::new(Rc::clone(&count));
        assert_eq!(count.get(), 2);
        drop(first);
        assert_eq!(count.get(), 1);
        drop(second);
        assert_eq!(count.get(), 0);
    }

    fn annexb(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in units {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(unit);
        }
        out
    }

    /// The profile, constraints and level are read off the parameter set
    /// rather than assumed, because configuring a high-profile stream as
    /// baseline is a decoder that either refuses or produces nothing usable.
    #[test]
    fn the_codec_string_comes_from_the_parameter_set() {
        // NAL type 7 (SPS), then profile 0x64 (high), constraints 0x00,
        // level 0x1F (3.1).
        let sps_pps = annexb(&[&[0x67, 0x64, 0x00, 0x1F, 0xAC], &[0x68, 0xEE, 0x3C, 0x80]]);
        assert_eq!(codec_string(&sps_pps).as_deref(), Some("avc1.64001F"));
    }

    /// A stream with no sequence parameter set has nothing to configure from,
    /// and answering `None` is what sends the caller down the "cannot decode
    /// here" path rather than into a misconfigured decoder.
    #[test]
    fn a_stream_with_no_parameter_set_has_no_codec_string() {
        assert_eq!(codec_string(&annexb(&[&[0x68, 0xEE]])), None);
        assert_eq!(codec_string(&[]), None);
    }

    /// The parameter set is found among other units rather than only at the
    /// front: a call's first access unit carries the sets ahead of a slice.
    #[test]
    fn the_parameter_set_is_found_behind_other_units() {
        let stream = annexb(&[
            &[0x09, 0x10],
            &[0x67, 0x42, 0xC0, 0x1E, 0xAA],
            &[0x65, 0x88],
        ]);
        assert_eq!(codec_string(&stream).as_deref(), Some("avc1.42C01E"));
    }
}
