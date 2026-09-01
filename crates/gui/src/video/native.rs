//! A video attachment, decoded a frame at a time.
//!
//! Pulled by index rather than pushed: the timeline asks for the frame it is
//! about to draw, so what is held is the *compressed* samples plus one
//! decoded picture, and an unplayed video in a scrolled-past bubble costs
//! what its file costs. A whole decode up front is the same video as tens of
//! megabytes of YUV, per attachment, in a window that has no idea which of
//! them anybody will watch.
//!
//! The container above it is [`super::demux`], which is the same reader the
//! browser's decoder uses: only the decode differs, so only the decode is
//! here.
//!
//! Decode is openh264 and the YUV→RGBA conversion is `YUVSource::write_rgba8`
//! (SIMD where available), which is the pipeline Zed's livekit_client uses on
//! Linux and for the same reason: upstream GPUI has no YUV surface there, so
//! the macOS `CVPixelBuffer` route has no counterpart and the convert happens
//! on the CPU.
//!
//! Nothing here writes an INFO line. Opening an attachment parses a container
//! and reads a parameter set, and every one of those numbers is derived from
//! a file somebody sent: worth having when a video will not play, and not
//! worth a dozen lines in the journal every time one is opened.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use smallvec::SmallVec;

use super::audio::VideoAudio;
use super::demux::{StreamingFrame, Track, nal_types};
use super::geometry::{
    MAX_VIDEO_PIXELS, declares_more_than, declares_unreadably, frame_byte_len, write_bgra_rotated,
};

/// Streaming video decoder that decodes frames on-demand.
pub struct StreamingVideoDecoder {
    /// The container's video track: its samples, its parameter sets and its
    /// timing, read by the demux both targets share.
    track: Track,
    /// Current decoder state
    decoder: Decoder,
    /// Index of last decoded frame (-1 if none)
    last_decoded_index: i32,
    /// Currently decoded frame (only 1 in memory)
    current_frame: Option<StreamingFrame>,
    /// Reusable RGBA scratch the decoder writes into, kept across frames: the
    /// buffer handed to `RenderImage` is a second one, because the channel
    /// swap and the rotation both read the source while writing elsewhere.
    ///
    /// Resized when a frame's own geometry says to; see [`frame_byte_len`].
    rgba_buffer: Vec<u8>,
    /// What the last frame was, so an unchanged one reuses the buffer.
    frame_size: (usize, usize),
}

impl StreamingVideoDecoder {
    /// Create a new streaming video decoder from MP4 data
    pub fn new(mp4_data: &[u8]) -> Result<Self> {
        // The declared frame is bounded before the container is walked; see
        // [`Track::read`].
        let track = Track::read(mp4_data)?;

        // Calculate memory savings
        let compressed_size: usize = track.samples.iter().map(|s| s.data.len()).sum();
        let pixels = track.width as usize * track.height as usize;
        let yuv_frame_size = (pixels * 3) / 2; // YUV420 = 1.5 bytes/pixel
        let bgra_frame_size = pixels * 4;
        log::debug!(
            "StreamingVideoDecoder: H.264={} KB, YUV frame={} KB (vs {} KB BGRA, {:.0}% savings)",
            compressed_size / 1024,
            yuv_frame_size / 1024,
            bgra_frame_size / 1024,
            (1.0 - yuv_frame_size as f64 / bgra_frame_size as f64) * 100.0
        );

        // The container's declaration was bounded above; this is the number
        // the decoder would actually allocate from, read before it is handed
        // one.
        if let Some((coded_width, coded_height)) =
            declares_more_than(&track.sps_pps, MAX_VIDEO_PIXELS)
        {
            return Err(anyhow!(
                "Coded video dimensions out of range: {}x{}",
                coded_width,
                coded_height
            ));
        }
        if declares_unreadably(&track.sps_pps) {
            return Err(anyhow!("Coded video dimensions could not be read"));
        }

        // Create decoder
        let decoder = Decoder::new().context("Failed to create H.264 decoder")?;

        Ok(Self {
            track,
            decoder,
            last_decoded_index: -1,
            current_frame: None,
            // Sized by the first frame that arrives, from its own geometry.
            rgba_buffer: Vec::new(),
            frame_size: (0, 0),
        })
    }

    /// Get total number of frames
    pub fn frame_count(&self) -> usize {
        self.track.frame_count()
    }

    /// Get video duration
    pub fn duration(&self) -> Duration {
        self.track.duration
    }

    /// Seek to a specific time and decode that frame
    pub fn seek(&mut self, time: Duration) {
        self.seek_to_frame(self.track.index_at(time));
    }

    /// Seek to a specific frame index
    pub fn seek_to_frame(&mut self, target_index: usize) {
        if target_index >= self.track.samples.len() {
            return;
        }

        // If we're already at this frame, no need to decode
        if let Some(ref frame) = self.current_frame
            && frame.index == target_index
        {
            return;
        }

        // Determine where to start decoding from
        let start_index = if target_index as i32 > self.last_decoded_index {
            // Moving forward - continue from where we are
            (self.last_decoded_index + 1) as usize
        } else {
            // Backwards: the decoder's state is no use, so it is rebuilt — but
            // from the keyframe the target is coded against rather than from
            // the start of the file. Each sample already knows whether it is
            // one; nothing asked. Dragging into the middle of three minutes at
            // thirty frames a second re-decoded about 2700 frames, on the
            // thread that draws the window.
            self.reset_decoder();
            self.track.keyframe_at_or_before(target_index)
        };

        // Decode frames from start_index to target_index
        for idx in start_index..=target_index {
            self.decode_frame(idx, idx == target_index);
        }
    }

    /// Reset decoder state (needed when seeking backward)
    fn reset_decoder(&mut self) {
        // Create new decoder instance
        if let Ok(new_decoder) = Decoder::new() {
            self.decoder = new_decoder;
            self.last_decoded_index = -1;

            // Feed SPS/PPS to initialize
            if !self.track.sps_pps.is_empty() {
                let _ = self.decoder.decode(&self.track.sps_pps);
            }
        }
    }

    /// Decode a single frame
    fn decode_frame(&mut self, index: usize, keep_output: bool) {
        let Some(sample) = self.track.samples.get(index) else {
            return;
        };

        let is_keyframe = sample.is_keyframe;
        let sample_size = sample.data.len();

        // Log first frame decode attempt
        if index == 0 {
            log::debug!(
                "Decoding first frame: keyframe={}, size={} bytes, keep_output={}",
                is_keyframe,
                sample_size,
                keep_output
            );
        }

        // A sample may declare a picture of its own, and it is read before
        // the decoder allocates from it. Not a gap to recover from: every
        // unit after this one references a picture that was never decoded,
        // so the stream stays refused until one that fits declares itself.
        if let Some((coded_width, coded_height)) =
            declares_more_than(&sample.data, MAX_VIDEO_PIXELS)
        {
            log::warn!("refusing a {coded_width}x{coded_height} video stream");
            self.last_decoded_index = index as i32;
            return;
        }
        if declares_unreadably(&sample.data) {
            log::warn!("refusing a video stream whose parameter set cannot be read");
            self.last_decoded_index = index as i32;
            return;
        }

        // For keyframes, feed SPS/PPS first
        if is_keyframe && !self.track.sps_pps.is_empty() {
            log::debug!("Feeding SPS/PPS before keyframe {}", index);
            let _ = self.decoder.decode(&self.track.sps_pps);
        }

        // Decode the sample
        match self.decoder.decode(&self.track.samples[index].data) {
            Ok(Some(yuv)) => {
                self.last_decoded_index = index as i32;

                if index == 0 {
                    let (y_stride, u_stride, v_stride) = yuv.strides();
                    log::trace!(
                        "First frame decoded: strides=({}, {}, {}), plane sizes=({}, {}, {})",
                        y_stride,
                        u_stride,
                        v_stride,
                        yuv.y().len(),
                        yuv.u().len(),
                        yuv.v().len()
                    );
                }

                // Only materialize a frame if the caller wants to keep it
                if keep_output {
                    let (frame_width, frame_height) = yuv.dimensions();
                    let Some(byte_len) = frame_byte_len(frame_width, frame_height) else {
                        log::warn!("refusing a {frame_width}x{frame_height} video frame");
                        return;
                    };
                    if self.frame_size != (frame_width, frame_height) {
                        self.rgba_buffer = vec![0u8; byte_len];
                        self.frame_size = (frame_width, frame_height);
                    }

                    // openh264 writes RGBA directly (SIMD path `write_rgba8_f32x8`
                    // when the host supports it, scalar fallback otherwise).
                    yuv.write_rgba8(&mut self.rgba_buffer);

                    // `RenderImage` reads the buffer as BGRA, and the frame
                    // still has to be turned the way the track says.
                    let rotation = self.track.rotation;
                    let mut owned = vec![0u8; byte_len];
                    write_bgra_rotated(
                        &self.rgba_buffer,
                        frame_width,
                        frame_height,
                        rotation,
                        &mut owned,
                    );
                    let (display_width, display_height) = if rotation.transposes() {
                        (frame_height, frame_width)
                    } else {
                        (frame_width, frame_height)
                    };
                    let Some(image) =
                        RgbaImage::from_raw(display_width as u32, display_height as u32, owned)
                    else {
                        log::warn!(
                            "Frame {}: RgbaImage::from_raw failed (size mismatch)",
                            index
                        );
                        return;
                    };
                    let render_image =
                        Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(image), 1)));

                    self.current_frame = Some(StreamingFrame {
                        image: render_image,
                        timestamp: self.track.timestamp_of(index),
                        index,
                    });

                    if index == 0 {
                        log::debug!(
                            "First frame BGRA created: {} bytes ({}x{}, rotation={:?})",
                            byte_len,
                            display_width,
                            display_height,
                            rotation
                        );
                    }
                }
            }
            Ok(None) => {
                // Decoder needs more data (buffering)
                self.last_decoded_index = index as i32;
                if index == 0 {
                    log::warn!(
                        "First frame returned None (decoder buffering) - may need more data"
                    );
                }
            }
            Err(e) => {
                // Get NAL types for debugging
                let nal_types = nal_types(&self.track.samples[index].data);
                let error_str = format!("{}", e);

                // Parse native error code and provide human-readable explanation
                let error_explanation = if error_str.contains("Native:") {
                    // Extract native code from error string like "Native:16"
                    let native_code = error_str
                        .split("Native:")
                        .nth(1)
                        .and_then(|s| s.split('.').next())
                        .and_then(|s| s.trim().parse::<i32>().ok())
                        .unwrap_or(-1);

                    match native_code {
                        1 => "dsFramePending - decoder needs more data, not enough NAL units",
                        2 => "dsRefLost - reference frame lost, may need to seek to keyframe",
                        3 => "dsBitstreamError - corrupted bitstream or invalid NAL",
                        4 => "dsDepLayerLost - dependency layer lost",
                        5 => "dsNoParamSets - missing SPS/PPS parameter sets",
                        6 => "dsDataErrorConcealed - error concealed, frame may be corrupted",
                        16 => {
                            "dsInvalidArgument - invalid data passed to decoder (possibly wrong NAL format or corrupted frame)"
                        }
                        32 => "dsInitialOptExpected - initialization option expected",
                        64 => "dsOutOfMemory - decoder ran out of memory",
                        _ => "unknown error code",
                    }
                } else {
                    "see error details above"
                };

                log::warn!(
                    "Failed to decode frame {} (keyframe={}, size={} bytes, NAL types={:?}): {} - {}",
                    index,
                    is_keyframe,
                    sample_size,
                    nal_types,
                    e,
                    error_explanation
                );

                // If this is after many consecutive failures, it might indicate a codec issue
                if index > 0 && index.is_multiple_of(100) {
                    log::warn!(
                        "Multiple decode failures - video may use unsupported H.264 features (B-frames, high profile, etc.)"
                    );
                }

                self.last_decoded_index = index as i32;
            }
        }
    }

    /// Why the decoder stopped, if it has.
    ///
    /// Never: this decoder reports a refused sample where it happens and
    /// carries no failure past it. The browser's is asynchronous and does,
    /// so the player asks both and only one ever answers.
    pub fn failure(&self) -> Option<String> {
        None
    }

    /// Get current decoded frame
    pub fn current_frame(&self) -> Option<&StreamingFrame> {
        self.current_frame.as_ref()
    }

    /// Give up whatever the platform is holding while nothing plays.
    ///
    /// Nothing, here: openh264 is a decoder in this process's own memory,
    /// with no session to run out of. The web twin gives back a hardware
    /// codec session, which a browser allows only a handful of. Present on
    /// both so the caller does not learn which build it is in.
    pub fn release(&mut self) {}

    /// Reset to first frame
    pub fn reset(&mut self) {
        self.reset_decoder();
        self.current_frame = None;
        self.seek_to_frame(0);
    }

    /// Take the audio data (consumes it from the decoder)
    pub fn take_audio(&mut self) -> Option<VideoAudio> {
        self.track.audio.take()
    }
}

#[cfg(test)]
mod tests {
    use super::super::geometry::Rotation;
    use super::*;

    /// The crash a `.mp4` used to cause, and the sizing that stops it.
    ///
    /// `avc1` declares a size and the picture is coded against another; a
    /// remux or a crop is enough. openh264 asserts that the RGBA target
    /// matches what it decoded, so a buffer taken from the declaration ends
    /// the process rather than the playback.
    ///
    /// Encoded and decoded for real, because a hand-built picture would only
    /// prove that this test agrees with itself.
    #[test]
    fn a_frame_larger_than_the_container_declared_does_not_panic() {
        use openh264::encoder::{Encoder, EncoderConfig};
        use openh264::formats::{RgbSliceU8, YUVBuffer};

        let (coded_width, coded_height) = (128usize, 96usize);
        let mut encoder =
            Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new())
                .expect("encoder");
        let pixels = vec![0u8; coded_width * coded_height * 3];
        let unit = encoder
            .encode(&YUVBuffer::from_rgb8_source(RgbSliceU8::new(
                &pixels,
                (coded_width, coded_height),
            )))
            .expect("encode")
            .to_vec();

        let mut decoder = Decoder::new().expect("decoder");
        let yuv = decoder
            .decode(&unit)
            .expect("decode")
            .expect("a keyframe decodes to a picture");
        let (width, height) = yuv.dimensions();

        // What the container claimed, which is not what came out.
        let declared = (64usize, 48usize);
        assert_ne!(declared, (width, height));
        assert_ne!(
            frame_byte_len(declared.0, declared.1),
            frame_byte_len(width, height),
            "the two sizes have to disagree for this to be testing anything"
        );

        // The call that used to end the window, against the buffer this
        // pipeline now hands it.
        let byte_len = frame_byte_len(width, height).expect("within budget");
        let mut rgba = vec![0u8; byte_len];
        yuv.write_rgba8(&mut rgba);

        let mut turned = vec![0u8; byte_len];
        write_bgra_rotated(&rgba, width, height, Rotation::None, &mut turned);
        assert!(
            RgbaImage::from_raw(width as u32, height as u32, turned).is_some(),
            "the image is drawn at the size it decoded at"
        );
    }

    /// A budget read off the container is one a sample walks past: an `avc1`
    /// entry can declare a thumbnail and the picture in front of it declare
    /// 16K, and openh264 allocates from whichever set it saw last.
    ///
    /// Encoded rather than hand-built, for the reason above.
    #[test]
    fn a_sample_declaring_its_own_picture_is_bounded_before_the_decoder_sees_it() {
        use openh264::encoder::{Encoder, EncoderConfig};
        use openh264::formats::{RgbSliceU8, YUVBuffer};

        let (width, height) = (128usize, 96usize);
        let mut encoder =
            Encoder::with_api_config(openh264::OpenH264API::from_source(), EncoderConfig::new())
                .expect("encoder");
        let pixels = vec![0u8; width * height * 3];
        let unit = encoder
            .encode(&YUVBuffer::from_rgb8_source(RgbSliceU8::new(
                &pixels,
                (width, height),
            )))
            .expect("encode")
            .to_vec();

        assert_eq!(
            declares_more_than(&unit, MAX_VIDEO_PIXELS),
            None,
            "an ordinary picture is decoded"
        );
        assert_eq!(
            declares_more_than(&unit, 64 * 48),
            Some((width as u32, height as u32)),
            "one past the budget is refused by what it declares, not by what it decoded to"
        );
        // A unit that declares nothing is decoded against the set before it.
        assert_eq!(declares_more_than(b"nothing here", MAX_VIDEO_PIXELS), None);
        assert!(!declares_unreadably(b"nothing here"));
    }

    /// The way past the budget is a parameter set shaped so the parser gives
    /// up: it declares geometry, so it is not the "decoded against the set
    /// before it" case, and nothing here can check what the decoder is about
    /// to allocate from it.
    #[test]
    fn a_parameter_set_that_cannot_be_read_is_refused() {
        let truncated = [0, 0, 0, 1, 0x67, 0x42];
        assert!(declares_unreadably(&truncated));
        assert_eq!(
            declares_more_than(&truncated, MAX_VIDEO_PIXELS),
            None,
            "and it is not a size, which is why it needs its own answer"
        );
    }
}
