//! Streaming video decoder with on-demand frame decoding.
//!
//! Same pipeline Zed's livekit_client uses on Linux: decode H.264 with
//! openh264, let `YUVSource::write_rgba8` do the YUV→RGBA conversion
//! (SIMD-accelerated when available), then wrap the RGBA buffer in a
//! [`gpui::RenderImage`]. Upstream GPUI has no YUV surface on Linux — the
//! macOS `CVPixelBuffer` path is the only hardware-accelerated route, so
//! doing the convert in CPU here matches what Zed itself does.

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use gpui::RenderImage;
use image::{Frame, RgbaImage};
use mp4::{Mp4Reader, TrackType};
use openh264::decoder::Decoder;
use openh264::formats::YUVSource;
use smallvec::SmallVec;

use super::audio::VideoAudio;

/// NAL unit start code for Annex B format
const NAL_START_CODE: &[u8] = &[0x00, 0x00, 0x00, 0x01];

/// Largest frame we will allocate an RGBA buffer for (8K). `width`/`height`
/// come from downloaded media, so their product is attacker-influenced.
const MAX_VIDEO_PIXELS: usize = 7680 * 4320;

/// Display rotation carried by the track's transformation matrix. A phone
/// records in its sensor's orientation and writes the correction here, so a
/// portrait clip decodes as landscape and only the matrix says which way is
/// up. Angles are clockwise, as applied when drawing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Rotation {
    None,
    Cw90,
    Cw180,
    Cw270,
}

const ONE: i32 = 0x0001_0000;
const NEG_ONE: i32 = -ONE;

impl Rotation {
    /// Classify the upper-left 2x2 of the ISO 14496-12 matrix. Its entries are
    /// 16.16 fixed point; only the quarter turns are representable as a pixel
    /// move, so anything else (a flip, a shear, a scale) is left alone.
    fn from_matrix(a: i32, b: i32, c: i32, d: i32) -> Self {
        match (a, b, c, d) {
            (0, ONE, NEG_ONE, 0) => Self::Cw90,
            (NEG_ONE, 0, 0, NEG_ONE) => Self::Cw180,
            (0, NEG_ONE, ONE, 0) => Self::Cw270,
            _ => Self::None,
        }
    }

    /// A count of quarter turns clockwise. How a call's peer states the
    /// rotation of their *device* — which is not the turn that draws their
    /// picture; see [`Rotation::to_upright`]. Anything outside `0..=3` is not
    /// a rotation, and is left alone rather than guessed at.
    #[cfg(test)]
    pub(super) fn from_quarter_turns(turns: u8) -> Self {
        match turns {
            1 => Self::Cw90,
            2 => Self::Cw180,
            3 => Self::Cw270,
            _ => Self::None,
        }
    }

    /// The turn that draws a peer's frame the right way up, given the
    /// `device_orientation` they announced.
    ///
    /// Their rotation *undone*, not repeated. A camera encodes in its sensor's
    /// orientation whatever the device is doing, so the picture arrives
    /// already turned by however the phone is held, and
    /// `device_orientation` is the description of that turn rather than a
    /// correction for it. Applying it again is what put a peer holding their
    /// phone sideways on their head: one quarter turn the wrong way is 180°
    /// out, which is the one error a wrong sign can make look like a
    /// deliberate choice.
    pub(super) fn to_upright(device_orientation: u8) -> Self {
        match device_orientation {
            1 => Self::Cw270,
            2 => Self::Cw180,
            3 => Self::Cw90,
            _ => Self::None,
        }
    }

    /// Whether the rotation exchanges width and height.
    pub(super) fn transposes(self) -> bool {
        matches!(self, Self::Cw90 | Self::Cw270)
    }
}

/// Copy `src` (RGBA, `width` x `height`) into `dst` as BGRA, applying `rotation`.
///
/// Two corrections in one pass: `RenderImage` is BGRA and openh264 writes
/// RGBA, and the frame has to be turned by the track matrix. `dst` holds the
/// same bytes laid out in the destination geometry, which is the source's
/// transposed for a quarter turn.
pub(super) fn write_bgra_rotated(
    src: &[u8],
    width: usize,
    height: usize,
    rotation: Rotation,
    dst: &mut [u8],
) {
    debug_assert_eq!(src.len(), width * height * 4);
    debug_assert_eq!(dst.len(), src.len());

    let dst_width = if rotation.transposes() { height } else { width };

    for y in 0..height {
        for x in 0..width {
            let (dx, dy) = match rotation {
                Rotation::None => (x, y),
                Rotation::Cw90 => (height - 1 - y, x),
                Rotation::Cw180 => (width - 1 - x, height - 1 - y),
                Rotation::Cw270 => (y, width - 1 - x),
            };
            let s = (y * width + x) * 4;
            let t = (dy * dst_width + dx) * 4;
            dst[t] = src[s + 2];
            dst[t + 1] = src[s + 1];
            dst[t + 2] = src[s];
            dst[t + 3] = src[s + 3];
        }
    }
}

/// A decoded video frame, BGRA8-encoded and ready to hand to `gpui::img`.
#[derive(Clone)]
pub struct StreamingFrame {
    /// Decoded RGBA frame, converted from YUV to BGRA in CPU.
    pub image: Arc<RenderImage>,
    /// Presentation timestamp
    pub timestamp: Duration,
    /// Frame index
    pub index: usize,
}

/// H.264 sample in Annex B format (ready for decoder)
struct H264Sample {
    /// NAL units in Annex B format
    data: Vec<u8>,
    /// Whether this is a keyframe (IDR)
    is_keyframe: bool,
}

/// Streaming video decoder that decodes frames on-demand.
pub struct StreamingVideoDecoder {
    /// H.264 samples (Annex B format) - compressed, small
    samples: Vec<H264Sample>,
    /// SPS/PPS NAL units (needed to initialize decoder)
    sps_pps: Vec<u8>,
    /// Coded frame dimensions, as the decoder produces them.
    width: u32,
    height: u32,
    /// Display rotation from the track matrix, applied to every kept frame.
    rotation: Rotation,
    /// Frame duration
    frame_duration: Duration,
    /// Total video duration
    duration: Duration,
    /// Current decoder state
    decoder: Decoder,
    /// Index of last decoded frame (-1 if none)
    last_decoded_index: i32,
    /// Currently decoded frame (only 1 in memory)
    current_frame: Option<StreamingFrame>,
    /// Decoded audio from the video
    audio: Option<VideoAudio>,
    /// Reusable RGBA scratch the decoder writes into, kept across frames: the
    /// buffer handed to `RenderImage` is a second one, because the channel
    /// swap and the rotation both read the source while writing elsewhere.
    rgba_buffer: Vec<u8>,
    /// Precomputed `width * height * 4`; size of both buffers.
    rgba_byte_len: usize,
}

impl StreamingVideoDecoder {
    /// Create a new streaming video decoder from MP4 data
    pub fn new(mp4_data: &[u8]) -> Result<Self> {
        log::info!(
            "StreamingVideoDecoder: parsing MP4 data ({} bytes)",
            mp4_data.len()
        );

        let cursor = Cursor::new(mp4_data);
        let mp4 = Mp4Reader::read_header(cursor, mp4_data.len() as u64)
            .context("Failed to read MP4 header")?;

        // Log all tracks found
        log::info!("MP4 contains {} tracks:", mp4.tracks().len());
        for (id, track) in mp4.tracks() {
            let track_type = track
                .track_type()
                .map(|t| format!("{:?}", t))
                .unwrap_or_else(|_| "Unknown".to_string());
            log::info!(
                "  Track {}: type={}, media_type={:?}, codec={:?}",
                id,
                track_type,
                track.media_type(),
                track
                    .video_profile()
                    .map(|p| format!("{:?}", p))
                    .unwrap_or_else(|_| "N/A".to_string())
            );
        }

        // Find video track
        let video_track = mp4
            .tracks()
            .values()
            .find(|t| matches!(t.track_type(), Ok(TrackType::Video)))
            .ok_or_else(|| anyhow!("No video track found in MP4"))?;

        let track_id = video_track.track_id();
        let duration = video_track.duration();
        let sample_count = video_track.sample_count();

        // Calculate FPS and frame duration; guard sample_count so an empty track
        // can't produce fps=0 and panic Duration::from_secs_f64 with infinity
        let fps = if sample_count > 0 && duration.as_secs_f64() > 0.0 {
            sample_count as f64 / duration.as_secs_f64()
        } else {
            30.0
        };
        let frame_duration = Duration::from_secs_f64(1.0 / fps);

        // AVCC allows 1-, 2- or 4-byte NAL length prefixes; assuming 4 misparses
        // any valid file that uses a narrower one. The 3-byte encoding
        // (length_size_minus_one == 2) is reserved by ISO/IEC 14496-15.
        let nal_length_size = match video_track
            .trak
            .mdia
            .minf
            .stbl
            .stsd
            .avc1
            .as_ref()
            .map(|avc1| avc1.avcc.length_size_minus_one)
        {
            Some(0) => 1,
            Some(1) => 2,
            Some(3) => 4,
            Some(other) => {
                return Err(anyhow!("Unsupported avcC length_size_minus_one: {}", other));
            }
            None => return Err(anyhow!("Video track has no avcC configuration record")),
        };

        // Get video dimensions
        let width = video_track.width() as u32;
        let height = video_track.height() as u32;

        // The coded frame is the sensor's orientation; the track matrix is the
        // correction a phone writes instead of re-encoding.
        let matrix = &video_track.trak.tkhd.matrix;
        let rotation = Rotation::from_matrix(matrix.a, matrix.b, matrix.c, matrix.d);

        // Bound the frame before anything allocates width*height*4: a
        // malformed or hostile file must not be able to ask for a buffer that
        // overflows the multiply or kills the process.
        let pixel_count = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| anyhow!("Video dimensions overflow: {}x{}", width, height))?;
        if pixel_count == 0 || pixel_count > MAX_VIDEO_PIXELS {
            return Err(anyhow!(
                "Video dimensions out of range: {}x{} ({} pixels)",
                width,
                height,
                pixel_count
            ));
        }

        // Log detailed video track info
        log::info!(
            "Video track {}: {}x{}, {} samples, {:.2} fps, duration: {:.2}s",
            track_id,
            width,
            height,
            sample_count,
            fps,
            duration.as_secs_f64(),
        );
        log::info!(
            "Video track details: timescale={}, bitrate={} kbps, rotation={:?}",
            video_track.timescale(),
            video_track.bitrate() / 1000,
            rotation,
        );

        // Get SPS and PPS from the track
        let sps = video_track
            .sequence_parameter_set()
            .ok()
            .map(|s| s.to_vec());
        let pps = video_track.picture_parameter_set().ok().map(|s| s.to_vec());

        // Log SPS/PPS info
        log::info!(
            "SPS: {} bytes, PPS: {} bytes",
            sps.as_ref().map(|s| s.len()).unwrap_or(0),
            pps.as_ref().map(|s| s.len()).unwrap_or(0),
        );
        if let Some(ref sps_data) = sps
            && !sps_data.is_empty()
        {
            // Log first few bytes of SPS for debugging
            let preview: Vec<String> = sps_data
                .iter()
                .take(16)
                .map(|b| format!("{:02x}", b))
                .collect();
            log::debug!("SPS data (first 16 bytes): {}", preview.join(" "));

            // Parse H.264 profile from SPS (byte 1 after NAL header)
            // SPS NAL type is 7, so first byte is NAL header, then profile_idc
            if sps_data.len() >= 4 {
                let profile_idc = sps_data[1];
                let constraint_flags = sps_data[2];
                let level_idc = sps_data[3];

                let profile_name = match profile_idc {
                    66 => "Baseline",
                    77 => "Main",
                    88 => "Extended",
                    100 => "High",
                    110 => "High 10",
                    122 => "High 4:2:2",
                    244 => "High 4:4:4 Predictive",
                    _ => "Unknown",
                };

                log::info!(
                    "H.264 Profile: {} (profile_idc={}), Level: {}.{}, Constraints: 0x{:02x}",
                    profile_name,
                    profile_idc,
                    level_idc / 10,
                    level_idc % 10,
                    constraint_flags
                );

                // Warn about potentially problematic profiles
                if profile_idc >= 100 {
                    log::warn!(
                        "Video uses {} profile - OpenH264 may have limited support for advanced features",
                        profile_name
                    );
                }
            }
        }

        // Build SPS/PPS in Annex B format
        let sps_pps = Self::build_sps_pps_annexb(sps.as_deref(), pps.as_deref());
        log::info!("Built SPS/PPS Annex B data: {} bytes", sps_pps.len());

        // Extract H.264 samples (keep compressed)
        let samples = Self::extract_samples(mp4_data, track_id, sample_count, nal_length_size)?;

        // Calculate memory savings
        let compressed_size: usize = samples.iter().map(|s| s.data.len()).sum();
        let yuv_frame_size = (width as usize * height as usize * 3) / 2; // YUV420 = 1.5 bytes/pixel
        let bgra_frame_size = width as usize * height as usize * 4;
        log::info!(
            "StreamingVideoDecoder: H.264={} KB, YUV frame={} KB (vs {} KB BGRA, {:.0}% savings)",
            compressed_size / 1024,
            yuv_frame_size / 1024,
            bgra_frame_size / 1024,
            (1.0 - yuv_frame_size as f64 / bgra_frame_size as f64) * 100.0
        );

        // Create decoder
        let decoder = Decoder::new().context("Failed to create H.264 decoder")?;

        // Extract audio
        let audio = Self::extract_audio(mp4_data);

        // pixel_count is bounded above, so this cannot overflow.
        let rgba_byte_len = pixel_count * 4;

        Ok(Self {
            samples,
            sps_pps,
            width,
            height,
            rotation,
            frame_duration,
            duration,
            decoder,
            last_decoded_index: -1,
            current_frame: None,
            audio,
            rgba_buffer: vec![0u8; rgba_byte_len],
            rgba_byte_len,
        })
    }

    /// Extract H.264 samples from MP4 without decoding
    fn extract_samples(
        mp4_data: &[u8],
        track_id: u32,
        sample_count: u32,
        nal_length_size: usize,
    ) -> Result<Vec<H264Sample>> {
        let cursor = Cursor::new(mp4_data);
        let mut mp4 = Mp4Reader::read_header(cursor, mp4_data.len() as u64)?;

        let mut samples = Vec::with_capacity(sample_count as usize);
        let mut keyframe_count = 0;
        let mut total_size = 0usize;
        let mut failed_reads = 0;

        for sample_idx in 1..=sample_count {
            match mp4.read_sample(track_id, sample_idx) {
                Ok(Some(sample)) => {
                    // Log first sample's raw data for debugging
                    if sample_idx == 1 {
                        let preview: Vec<String> = sample
                            .bytes
                            .iter()
                            .take(32)
                            .map(|b| format!("{:02x}", b))
                            .collect();
                        log::debug!("First sample raw data (32 bytes): {}", preview.join(" "));
                        log::debug!("First sample size: {} bytes", sample.bytes.len());
                    }

                    // Convert AVCC to Annex B format
                    let annexb_data = Self::avcc_to_annexb(&sample.bytes, nal_length_size);

                    // Log NAL unit types in first sample
                    if sample_idx == 1 {
                        let nal_types = Self::get_nal_types(&annexb_data);
                        log::info!("First sample NAL types: {:?}", nal_types);
                    }

                    // Check if this is a keyframe by looking at NAL unit type
                    let is_keyframe = Self::is_keyframe(&annexb_data);
                    if is_keyframe {
                        keyframe_count += 1;
                    }
                    total_size += annexb_data.len();

                    samples.push(H264Sample {
                        data: annexb_data,
                        is_keyframe,
                    });
                }
                Ok(None) => {
                    failed_reads += 1;
                    log::warn!("Sample {} returned None", sample_idx);
                }
                Err(e) => {
                    failed_reads += 1;
                    log::warn!("Failed to read sample {}: {}", sample_idx, e);
                }
            }
        }

        log::info!(
            "Extracted {} samples: {} keyframes, {} failed reads, total size: {} KB",
            samples.len(),
            keyframe_count,
            failed_reads,
            total_size / 1024
        );

        if samples.is_empty() {
            return Err(anyhow!("No video samples could be extracted"));
        }

        // Log keyframe positions if there are issues
        if keyframe_count == 0 {
            log::warn!("No keyframes detected! Video may not decode correctly.");
        }

        Ok(samples)
    }

    /// Get all NAL unit types in the data (for debugging)
    fn get_nal_types(annexb_data: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 4 < annexb_data.len() {
            if annexb_data[i..i + 4] == [0, 0, 0, 1]
                && let Some(&byte) = annexb_data.get(i + 4)
            {
                types.push(byte & 0x1F);
            }
            i += 1;
        }
        types
    }

    /// Check if NAL units contain an IDR (keyframe)
    fn is_keyframe(annexb_data: &[u8]) -> bool {
        // Look for NAL unit type 5 (IDR slice)
        let mut i = 0;
        while i + 4 < annexb_data.len() {
            if annexb_data[i..i + 4] == [0, 0, 0, 1] {
                let nal_type = annexb_data.get(i + 4).map(|b| b & 0x1F).unwrap_or(0);
                if nal_type == 5 {
                    return true;
                }
            }
            i += 1;
        }
        false
    }

    /// Get total number of frames
    pub fn frame_count(&self) -> usize {
        self.samples.len()
    }

    /// Get video duration
    pub fn duration(&self) -> Duration {
        self.duration
    }

    /// Seek to a specific time and decode that frame
    pub fn seek(&mut self, time: Duration) {
        let target_index = (time.as_secs_f64() / self.frame_duration.as_secs_f64()) as usize;
        let target_index = target_index.min(self.samples.len().saturating_sub(1));
        self.seek_to_frame(target_index);
    }

    /// Seek to a specific frame index
    pub fn seek_to_frame(&mut self, target_index: usize) {
        if target_index >= self.samples.len() {
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
            // Moving backward - need to reset decoder and start from beginning
            self.reset_decoder();
            0
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
            if !self.sps_pps.is_empty() {
                let _ = self.decoder.decode(&self.sps_pps);
            }
        }
    }

    /// Decode a single frame
    fn decode_frame(&mut self, index: usize, keep_output: bool) {
        if index >= self.samples.len() {
            return;
        }

        let is_keyframe = self.samples[index].is_keyframe;
        let sample_size = self.samples[index].data.len();

        // Log first frame decode attempt
        if index == 0 {
            log::info!(
                "Decoding first frame: keyframe={}, size={} bytes, keep_output={}",
                is_keyframe,
                sample_size,
                keep_output
            );
        }

        // For keyframes, feed SPS/PPS first
        if is_keyframe && !self.sps_pps.is_empty() {
            log::debug!("Feeding SPS/PPS before keyframe {}", index);
            let _ = self.decoder.decode(&self.sps_pps);
        }

        // Decode the sample
        match self.decoder.decode(&self.samples[index].data) {
            Ok(Some(yuv)) => {
                self.last_decoded_index = index as i32;

                if index == 0 {
                    let (y_stride, u_stride, v_stride) = yuv.strides();
                    log::info!(
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
                    // openh264 writes RGBA directly (SIMD path `write_rgba8_f32x8`
                    // when the host supports it, scalar fallback otherwise).
                    yuv.write_rgba8(&mut self.rgba_buffer);

                    // `RenderImage` reads the buffer as BGRA, and the frame
                    // still has to be turned the way the track says.
                    let mut owned = vec![0u8; self.rgba_byte_len];
                    write_bgra_rotated(
                        &self.rgba_buffer,
                        self.width as usize,
                        self.height as usize,
                        self.rotation,
                        &mut owned,
                    );
                    let (display_width, display_height) = self.display_size();
                    let Some(image) = RgbaImage::from_raw(display_width, display_height, owned)
                    else {
                        log::warn!(
                            "Frame {}: RgbaImage::from_raw failed (size mismatch)",
                            index
                        );
                        return;
                    };
                    let render_image =
                        Arc::new(RenderImage::new(SmallVec::from_elem(Frame::new(image), 1)));

                    let timestamp = self.frame_duration * index as u32;
                    self.current_frame = Some(StreamingFrame {
                        image: render_image,
                        timestamp,
                        index,
                    });

                    if index == 0 {
                        log::info!(
                            "First frame BGRA created: {} bytes ({}x{}, rotation={:?})",
                            self.rgba_byte_len,
                            display_width,
                            display_height,
                            self.rotation
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
                let nal_types = Self::get_nal_types(&self.samples[index].data);
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

    /// Frame geometry as drawn: the coded size, transposed by a quarter turn.
    fn display_size(&self) -> (u32, u32) {
        if self.rotation.transposes() {
            (self.height, self.width)
        } else {
            (self.width, self.height)
        }
    }

    /// Get current decoded frame
    pub fn current_frame(&self) -> Option<&StreamingFrame> {
        self.current_frame.as_ref()
    }

    /// Reset to first frame
    pub fn reset(&mut self) {
        self.reset_decoder();
        self.current_frame = None;
        self.seek_to_frame(0);
    }

    /// Take the audio data (consumes it from the decoder)
    pub fn take_audio(&mut self) -> Option<VideoAudio> {
        self.audio.take()
    }

    /// Convert AVCC format NAL units to Annex B format
    fn avcc_to_annexb(avcc_data: &[u8], nal_length_size: usize) -> Vec<u8> {
        let mut annexb = Vec::with_capacity(avcc_data.len() + 128);
        let mut pos = 0;

        while pos + nal_length_size <= avcc_data.len() {
            let mut nal_len: usize = 0;
            for i in 0..nal_length_size {
                nal_len = (nal_len << 8) | (avcc_data[pos + i] as usize);
            }
            pos += nal_length_size;

            if pos + nal_len > avcc_data.len() {
                break;
            }

            annexb.extend_from_slice(NAL_START_CODE);
            annexb.extend_from_slice(&avcc_data[pos..pos + nal_len]);
            pos += nal_len;
        }

        annexb
    }

    /// Build Annex B format data from SPS and PPS
    fn build_sps_pps_annexb(sps: Option<&[u8]>, pps: Option<&[u8]>) -> Vec<u8> {
        let mut annexb = Vec::new();

        if let Some(sps_data) = sps
            && !sps_data.is_empty()
        {
            annexb.extend_from_slice(NAL_START_CODE);
            annexb.extend_from_slice(sps_data);
        }

        if let Some(pps_data) = pps
            && !pps_data.is_empty()
        {
            annexb.extend_from_slice(NAL_START_CODE);
            annexb.extend_from_slice(pps_data);
        }

        annexb
    }

    /// Extract audio from MP4
    fn extract_audio(mp4_data: &[u8]) -> Option<VideoAudio> {
        super::audio::extract_audio_from_mp4(mp4_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_matrix_is_no_rotation() {
        assert_eq!(Rotation::from_matrix(ONE, 0, 0, ONE), Rotation::None);
        // A horizontal flip is not a quarter turn and must not be mistaken for one.
        assert_eq!(Rotation::from_matrix(NEG_ONE, 0, 0, ONE), Rotation::None);
    }

    #[test]
    fn a_peers_orientation_is_read_as_a_quarter_turn() {
        assert_eq!(Rotation::from_quarter_turns(0), Rotation::None);
        assert_eq!(Rotation::from_quarter_turns(1), Rotation::Cw90);
        assert_eq!(Rotation::from_quarter_turns(2), Rotation::Cw180);
        assert_eq!(Rotation::from_quarter_turns(3), Rotation::Cw270);
        // Not a rotation: drawn as it arrived rather than turned by a guess.
        assert_eq!(Rotation::from_quarter_turns(9), Rotation::None);
    }

    /// `device_orientation` says how the *sender* is held, so drawing it
    /// upright means turning the picture back by that much — the other way.
    /// Turning it the same way lands a phone on its side at 180°, which is a
    /// peer standing on their head.
    #[test]
    fn a_peers_orientation_is_undone_rather_than_repeated() {
        assert_eq!(Rotation::to_upright(0), Rotation::None);
        assert_eq!(Rotation::to_upright(1), Rotation::Cw270);
        assert_eq!(Rotation::to_upright(2), Rotation::Cw180);
        assert_eq!(Rotation::to_upright(3), Rotation::Cw90);
        // Not a rotation: drawn as it arrived rather than turned by a guess.
        assert_eq!(Rotation::to_upright(9), Rotation::None);
    }

    /// The property the two of them have to have: a frame turned by the
    /// sender's own rotation and then by the correction is the frame again.
    #[test]
    fn undoing_a_senders_rotation_restores_the_picture() {
        for turns in 0..4u8 {
            let (width, height) = (3usize, 2usize);
            let src = tagged(width, height);
            let sent = Rotation::from_quarter_turns(turns);
            let mut once = vec![0u8; src.len()];
            write_bgra_rotated(&src, width, height, sent, &mut once);
            let (turned_width, turned_height) = if sent.transposes() {
                (height, width)
            } else {
                (width, height)
            };
            let mut back = vec![0u8; src.len()];
            // Two passes swap the channels twice, so this is the source again.
            write_bgra_rotated(
                &once,
                turned_width,
                turned_height,
                Rotation::to_upright(turns),
                &mut back,
            );
            assert_eq!(back, src, "a peer at {turns} quarter turns");
        }
    }

    #[test]
    fn quarter_turns_are_classified() {
        assert_eq!(Rotation::from_matrix(0, ONE, NEG_ONE, 0), Rotation::Cw90);
        assert_eq!(
            Rotation::from_matrix(NEG_ONE, 0, 0, NEG_ONE),
            Rotation::Cw180
        );
        assert_eq!(Rotation::from_matrix(0, NEG_ONE, ONE, 0), Rotation::Cw270);
    }

    /// One pixel per position, tagged by its index, so a move is visible.
    fn tagged(width: usize, height: usize) -> Vec<u8> {
        (0..width * height)
            .flat_map(|i| [i as u8, 0, 0, 255])
            .collect()
    }

    /// Red channel of each pixel, read back out of a BGRA buffer.
    fn reds(buf: &[u8]) -> Vec<u8> {
        buf.as_chunks::<4>().0.iter().map(|p| p[2]).collect()
    }

    #[test]
    fn no_rotation_still_swaps_red_and_blue() {
        let src = [10u8, 20, 30, 40];
        let mut dst = [0u8; 4];
        write_bgra_rotated(&src, 1, 1, Rotation::None, &mut dst);
        assert_eq!(dst, [30, 20, 10, 40]);
    }

    #[test]
    fn cw90_moves_the_top_left_pixel_to_the_top_right() {
        // 3x2 source, indices 0..6 laid out row-major.
        let src = tagged(3, 2);
        let mut dst = vec![0u8; src.len()];
        write_bgra_rotated(&src, 3, 2, Rotation::Cw90, &mut dst);
        // Destination is 2x3: columns become rows, bottom row first.
        assert_eq!(reds(&dst), vec![3, 0, 4, 1, 5, 2]);
    }

    #[test]
    fn cw270_is_the_inverse_of_cw90() {
        let src = tagged(3, 2);
        let mut once = vec![0u8; src.len()];
        write_bgra_rotated(&src, 3, 2, Rotation::Cw90, &mut once);
        let mut back = vec![0u8; src.len()];
        // The intermediate is BGRA, so turning it back swaps the channels again.
        write_bgra_rotated(&once, 2, 3, Rotation::Cw270, &mut back);
        assert_eq!(back, src);
    }

    #[test]
    fn cw180_reverses_the_pixels() {
        let src = tagged(3, 2);
        let mut dst = vec![0u8; src.len()];
        write_bgra_rotated(&src, 3, 2, Rotation::Cw180, &mut dst);
        assert_eq!(reds(&dst), vec![5, 4, 3, 2, 1, 0]);
    }
}
