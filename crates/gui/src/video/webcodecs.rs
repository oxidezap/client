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

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::RenderImage;
use image::{Frame, RgbaImage};
use smallvec::SmallVec;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

use super::geometry::{
    MAX_VIDEO_PIXELS, Rotation, declares_more_than, frame_byte_len, write_bgra_rotated,
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
    /// The rotation applied to every picture on the way out.
    rotation: Rotation,
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
        // Before anything is configured, for the reason the native decoder
        // asks before it allocates: the numbers come from a file somebody
        // sent, and a budget applied after the decoder has sized its own
        // buffers is applied after the allocation it exists to prevent.
        if let Some((width, height)) = declares_more_than(sps_pps, MAX_VIDEO_PIXELS) {
            return Err(format!("refusing a {width}x{height} video stream"));
        }
        let codec = codec_string(sps_pps)
            .ok_or_else(|| "no readable parameter set in this stream".to_string())?;

        let slot = Rc::new(RefCell::new(Slot::default()));

        let on_frame = {
            let slot = Rc::clone(&slot);
            Closure::<dyn FnMut(web_sys::VideoFrame)>::new(move |frame: web_sys::VideoFrame| {
                read_frame(frame, rotation, Rc::clone(&slot));
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
            rotation,
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
        if let Some((width, height)) = declares_more_than(access_unit, MAX_VIDEO_PIXELS) {
            self.slot.borrow_mut().failed =
                Some(format!("refusing a {width}x{height} video stream"));
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
        let _ = self.inner.reset();
        let mut slot = self.slot.borrow_mut();
        slot.newest = None;
        slot.produced = 0;
    }

    /// The rotation this decoder applies, for a caller sizing a picture.
    pub fn rotation(&self) -> Rotation {
        self.rotation
    }
}

impl Drop for Decoder {
    /// Close the decoder rather than leaving it to the collector.
    ///
    /// It holds a hardware decode session, and a tab that opens one per video
    /// in a conversation runs out of them long before it runs out of memory.
    fn drop(&mut self) {
        let _ = self.inner.close();
    }
}

/// Read the pixels out of a decoded frame and put them in the slot.
///
/// Asynchronous, because `copy_to` is: the frame is closed as soon as the
/// copy resolves, since an unclosed `VideoFrame` pins a decoder buffer and a
/// decoder that runs out of them stops producing.
fn read_frame(frame: web_sys::VideoFrame, rotation: Rotation, slot: Rc<RefCell<Slot>>) {
    let width = frame.coded_width() as usize;
    let height = frame.coded_height() as usize;
    let timestamp_micros = frame.timestamp() as i64;

    // The decoder's own geometry, never the container's. See
    // [`super::geometry::frame_byte_len`] for why that distinction is the one
    // that matters.
    let Some(byte_len) = frame_byte_len(width, height) else {
        frame.close();
        let mut slot = slot.borrow_mut();
        if slot.failed.is_none() {
            slot.failed = Some(format!("refusing a {width}x{height} video frame"));
        }
        return;
    };

    let options = web_sys::VideoFrameCopyToOptions::new();
    options.set_format(web_sys::VideoPixelFormat::Rgba);

    // Into a JS-side buffer rather than a `&mut [u8]` over wasm memory. The
    // copy resolves later, and the only Rust buffer that could back it is one
    // this function is about to move into an async block: the promise would
    // be writing through a pointer into memory that has since moved. A
    // `Uint8Array` is the browser's own and survives whatever this side does.
    let destination = js_sys::Uint8Array::new_with_length(byte_len as u32);
    // Returns the promise directly rather than a `Result`: a `copyTo` that
    // cannot be started rejects rather than throwing, so there is one failure
    // path and it is the awaited one below.
    let promise = frame.copy_to_with_buffer_source_and_options(&destination, &options);

    wasm_bindgen_futures::spawn_local(async move {
        let read = wasm_bindgen_futures::JsFuture::from(promise).await;
        // Closed on both paths, and before the slot is touched: the buffer it
        // holds is the decoder's, not ours.
        frame.close();
        if let Err(e) = read {
            let mut slot = slot.borrow_mut();
            if slot.failed.is_none() {
                slot.failed = Some(format!("could not read a decoded frame: {e:?}"));
            }
            return;
        }

        let source = destination.to_vec();
        if source.len() != byte_len {
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

        let mut slot = slot.borrow_mut();
        slot.newest = Some(Picture {
            image,
            timestamp_micros,
        });
        slot.produced += 1;
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
