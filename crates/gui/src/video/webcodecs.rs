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
    write_bgra_rotated,
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
    /// How many pictures have come out, which is how a caller waiting for the
    /// first one knows it has arrived.
    produced: u64,
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
    /// How many copies have been started and not yet resolved.
    ///
    /// The callers bound the decoder's *input* queue, which says nothing
    /// about frames that have already left it. Reading the pixels out of one
    /// is asynchronous and allocates the whole picture, so a browser that
    /// decodes faster than it copies accumulates multi-megabyte buffers for
    /// as long as a call lasts, however few of them anybody draws.
    in_flight: Rc<Cell<usize>>,
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
        let turns: Rc<RefCell<TurnLog>> = Rc::new(RefCell::new(TurnLog::default()));

        let on_frame = {
            let slot = Rc::clone(&slot);
            let rotation = Rc::clone(&rotation);
            let turns = Rc::clone(&turns);
            let generation = Rc::clone(&generation);
            let submitted = Rc::clone(&submitted);
            let in_flight = Rc::clone(&in_flight);
            Closure::<dyn FnMut(web_sys::VideoFrame)>::new(move |frame: web_sys::VideoFrame| {
                // Dropped rather than queued, which is what every queue on
                // this path does: the slot holds one picture, so a frame
                // arriving while that many copies are still outstanding is
                // one nobody was going to see. Closing it is not optional
                // either, since an unclosed `VideoFrame` pins a decoder
                // buffer and a decoder that runs out stops producing.
                if in_flight.get() >= MAX_COPIES_IN_FLIGHT {
                    frame.close();
                    return;
                }
                let seq = submitted.get().wrapping_add(1);
                submitted.set(seq);
                // The turn this picture was encoded under, not whatever the
                // peer has done since. Falls back to the current one for a
                // picture whose stamp was never recorded, which is the
                // attachment path, where the turn never changes anyway.
                let turn = turns
                    .borrow_mut()
                    .take(frame.timestamp() as i32)
                    .unwrap_or_else(|| rotation.get());
                read_frame(
                    frame,
                    turn,
                    max_pixels,
                    Rc::clone(&slot),
                    sink.clone(),
                    Stamp {
                        generation: Rc::clone(&generation),
                        born: generation.get(),
                        seq,
                    },
                    Rc::clone(&in_flight),
                );
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

        let config = web_sys::VideoDecoderConfig::new(&codec);
        // Left to the browser: it knows its own hardware, and the picture is
        // read back through `copy_to` either way.
        inner
            .configure(&config)
            .map_err(|e| format!("the browser would not decode {codec}: {e:?}"))?;

        Ok(Self {
            inner,
            slot,
            codec,
            generation,
            submitted,
            in_flight,
            rotation,
            turns,
            max_pixels,
            _on_frame: on_frame,
            _on_error: on_error,
        })
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

    /// How many pictures have come out so far.
    pub fn produced(&self) -> u64 {
        self.slot.borrow().produced
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
        self.submitted.set(0);

        let _ = self.inner.reset();
        self.turns.borrow_mut().clear();
        {
            let mut slot = self.slot.borrow_mut();
            slot.newest = None;
            slot.produced = 0;
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
        let _ = self.inner.close();
    }
}

/// How many pixel copies may be outstanding at once.
///
/// A little more than the deepest input queue either caller allows, so an
/// ordinary burst is never refused and a browser that has stopped resolving
/// copies stops costing memory. The slot holds one picture, so what is
/// dropped here is a picture nobody would have drawn.
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
}

/// Read the pixels out of a decoded frame and put them in the slot.
///
/// Asynchronous, because `copy_to` is: the frame is closed as soon as the
/// copy resolves, since an unclosed `VideoFrame` pins a decoder buffer and a
/// decoder that runs out of them stops producing.
fn read_frame(
    frame: web_sys::VideoFrame,
    rotation: Rotation,
    max_pixels: usize,
    slot: Rc<RefCell<Slot>>,
    sink: Option<Rc<dyn Fn(Picture)>>,
    stamp: Stamp,
    in_flight: Rc<Cell<usize>>,
) {
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

    let options = web_sys::VideoFrameCopyToOptions::new();
    options.set_format(web_sys::VideoPixelFormat::Rgba);

    // The decoder's own geometry, never the container's. See
    // [`super::geometry::frame_byte_len`] for why that distinction is the one
    // that matters.
    let Some(byte_len) =
        frame_byte_len(width, height).filter(|_| width.saturating_mul(height) <= max_pixels)
    else {
        frame.close();
        if stamp.current() {
            let mut slot = slot.borrow_mut();
            if slot.failed.is_none() {
                slot.failed = Some(format!("refusing a {width}x{height} video frame"));
            }
        }
        return;
    };

    // Into a JS-side buffer rather than a `&mut [u8]` over wasm memory. The
    // copy resolves later, and the only Rust buffer that could back it is one
    // this function is about to move into an async block: the promise would
    // be writing through a pointer into memory that has since moved. A
    // `Uint8Array` is the browser's own and survives whatever this side does.
    // Asked of the frame rather than computed, because the browser knows its
    // own layout: `byte_len` above is the budget's arithmetic and this is the
    // buffer the copy will actually fill. They agree for packed RGBA, and
    // where they do not it is the browser that is right.
    let needed = frame
        .allocation_size_with_options(&options)
        .map_or(byte_len, |size| size as usize);
    if needed > byte_len {
        frame.close();
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
    let destination = js_sys::Uint8Array::new_with_length(needed as u32);
    // Counted from here, where the buffer is actually allocated, to wherever
    // the copy settles below. The two early returns above allocate nothing.
    let outstanding = Outstanding::new(in_flight);
    // Returns the promise directly rather than a `Result`: a `copyTo` that
    // cannot be started rejects rather than throwing, so there is one failure
    // path and it is the awaited one below.
    let promise = frame.copy_to_with_buffer_source_and_options(&destination, &options);

    wasm_bindgen_futures::spawn_local(async move {
        // Held for the length of the copy and released however it ends, so a
        // rejected or refused read frees the slot it took.
        let _outstanding = outstanding;
        let read = wasm_bindgen_futures::JsFuture::from(promise).await;
        // Closed on both paths, and before the slot is touched: the buffer it
        // holds is the decoder's, not ours.
        frame.close();
        if let Err(e) = read {
            if stamp.current() {
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
        if !stamp.current() {
            return;
        }

        let source = destination.to_vec();
        if source.len() < width * height * 4 {
            return;
        }
        let mut bgra = vec![0u8; byte_len];
        write_bgra_rotated(&source, width, height, rotation, &mut bgra);
        let (draw_width, draw_height) = if rotation.transposes() {
            (height, width)
        } else {
            (width, height)
        };
        let Some(buffer) = RgbaImage::from_raw(draw_width as u32, draw_height as u32, bgra) else {
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
            if !stamp.current() || stamp.seq <= slot.accepted {
                return;
            }
            slot.accepted = stamp.seq;
            slot.newest = Some(picture.clone());
            slot.produced += 1;
        }
        // After the borrow is released: a sink is the caller's code, and one
        // that asked this decoder anything would find it already borrowed.
        if let Some(sink) = sink {
            sink(picture);
        }
    });
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
