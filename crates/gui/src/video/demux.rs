//! Getting H.264 out of an MP4, for whichever decoder is going to read it.
//!
//! The container work is the same on both targets — `mp4` builds for
//! `wasm32-unknown-unknown` and the byte shuffling below is plain Rust — so
//! it is the *decoder* that differs, not the demux. Keeping these here is
//! what lets the browser path be a decoder swap rather than a second reader.
//!
//! [`Track`] is the whole of that reading, written once. It was written twice
//! for a while — once in the desktop decoder and once in the browser's — and
//! the two copies drifted: the bound that stops a container declaring four
//! billion samples was added to one of them and not the other, so the desktop
//! went on reserving from a number a file chose. One reader is what makes
//! that impossible rather than remembered.
//!
//! Everything is Annex B on the way out. AVCC is what the container stores
//! and neither decoder wants it: openh264 takes start codes, and a WebCodecs
//! configuration with no `description` is Annex B by specification.

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use gpui::RenderImage;
use mp4::{Mp4Reader, TrackType};

use super::audio::VideoAudio;
use super::geometry::{MAX_VIDEO_PIXELS, Rotation};

/// NAL unit start code for Annex B format.
pub(super) const NAL_START_CODE: &[u8] = &[0x00, 0x00, 0x00, 0x01];

/// One access unit, and whether the stream can be entered at it.
pub(super) struct H264Sample {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
}

/// The most samples a track may declare before it is refused.
///
/// A file's length is not the bound it looks like. `stsz` can declare a
/// *fixed* sample size of one byte, so a file inside the page's media budget
/// still names tens of millions of samples, and the cost is not the payload
/// but the bookkeeping: one `Vec` per sample is twenty-four bytes of metadata
/// before a byte of it is read, which turns a 48 MiB attachment into more than
/// a gigabyte and aborts a linear memory that has a ceiling.
///
/// A million is far past anything anybody sends: over nine hours of video at
/// 30 fps, and more than five of audio at the 47 AAC frames a second 48 kHz
/// gives. What it buys is that the metadata is bounded in the tens of
/// megabytes whatever the file claims.
pub(super) const MAX_TRACK_SAMPLES: usize = 1_000_000;

/// Rewrite AVCC length-prefixed units as Annex B start-code units.
///
/// `nal_length_size` is the container's, and it is 1, 2 or 4: assuming 4
/// misparses any valid file that uses a narrower prefix.
pub(super) fn avcc_to_annexb(avcc_data: &[u8], nal_length_size: usize) -> Vec<u8> {
    let mut annexb = Vec::with_capacity(avcc_data.len() + 16);
    let mut pos = 0;

    // Subtraction rather than `pos + nal_length_size <= len`: this target is
    // 32-bit and a four-byte prefix reads up to `0xffff_ffff`, so the sum is
    // one a malformed file can carry past `usize`. What that costs is not a
    // wrong answer but a panic in the slice below, on a file somebody sent.
    while avcc_data.len().saturating_sub(pos) >= nal_length_size {
        let mut nal_len = 0usize;
        for i in 0..nal_length_size {
            nal_len = (nal_len << 8) | avcc_data[pos + i] as usize;
        }
        pos += nal_length_size;

        // A length that runs past the buffer is a truncated or malformed
        // sample; what has been read so far is still decodable. So is one
        // that cannot be added to the position at all.
        if nal_len == 0 {
            break;
        }
        let Some(end) = pos
            .checked_add(nal_len)
            .filter(|end| *end <= avcc_data.len())
        else {
            break;
        };

        annexb.extend_from_slice(NAL_START_CODE);
        annexb.extend_from_slice(&avcc_data[pos..end]);
        pos = end;
    }

    annexb
}

/// The parameter sets, as the Annex B preamble a decoder is configured with.
pub(super) fn build_sps_pps_annexb(sps: Option<&[u8]>, pps: Option<&[u8]>) -> Vec<u8> {
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

/// Whether this access unit carries an IDR, which is where a decode may start.
pub(super) fn is_keyframe(annexb_data: &[u8]) -> bool {
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

/// The first sample at or before `index` a decode may be entered at.
///
/// A decoder's reference chain only runs forwards, so a backward seek has to
/// re-enter the stream at a keyframe and replay to the target. A stream whose
/// first sample is not an IDR still has to start somewhere, and the start is
/// the only honest answer.
pub(super) fn keyframe_at_or_before(samples: &[H264Sample], index: usize) -> usize {
    (0..=index)
        .rev()
        .find(|&i| samples.get(i).is_some_and(|s| s.is_keyframe))
        .unwrap_or(0)
}

/// The stamp a sample is fed under, which is its own index.
///
/// A WebCodecs timestamp is a label rather than a clock — nothing but this
/// side reads it — so the index is the one value that stays unique for the
/// whole track. Microseconds would not: the binding takes an `i32`, which
/// runs out around thirty-six minutes, and every frame past that would carry
/// the same stamp. A reader keying on the stamp to tell one picture from the
/// next then sees them all as the same picture and freezes on the first,
/// while playback goes on advancing.
///
/// The displayed position is computed from the index instead; see
/// `StreamingFrame::timestamp`.
// Read by the browser's decoder and by the tests below; a native build
// compiles neither caller.
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
pub(super) fn stamp_of(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

/// A decoded video frame, BGRA8-encoded and ready to hand to `gpui::img`.
///
/// One definition for both decoders: the player pulls by index and reads
/// exactly these three fields, so what produced the picture is not its
/// business.
#[derive(Clone)]
pub struct StreamingFrame {
    /// The picture, already turned the way the track says.
    pub image: Arc<RenderImage>,
    /// Where in the video it sits.
    pub timestamp: Duration,
    /// Frame index.
    pub index: usize,
}

/// The video track of an MP4: its samples, its parameter sets, and the numbers
/// a player needs to place them in time.
///
/// Read the same way on both targets, because the container is the same file
/// either side of the split. What differs is only what is done with
/// [`Self::samples`] afterwards.
pub struct Track {
    /// Every sample of the video track, Annex B, still compressed.
    pub samples: Vec<H264Sample>,
    /// The parameter sets as an Annex B preamble: what a decoder is
    /// configured with, and what rides with the first unit after a reset.
    pub sps_pps: Vec<u8>,
    /// Display rotation from the track matrix, applied to every kept frame.
    pub rotation: Rotation,
    /// How long one frame lasts, derived from the count and the duration.
    pub frame_duration: Duration,
    /// How long the whole track lasts.
    pub duration: Duration,
    /// What the container declares the picture to be, before any sample has
    /// declared something of its own.
    ///
    /// Bounded by [`Self::read`] before anything is demuxed, and read after
    /// that only by the decoder that allocates a buffer from it — which is
    /// the desktop's. The browser's takes its geometry from the stream, so on
    /// a page nothing asks.
    #[cfg_attr(target_family = "wasm", allow(dead_code, reason = "no decoder here"))]
    pub width: u32,
    /// The other half of that declaration.
    #[cfg_attr(target_family = "wasm", allow(dead_code, reason = "no decoder here"))]
    pub height: u32,
    /// The audio track beside it, already decoded, if there was one.
    pub audio: Option<VideoAudio>,
}

impl Track {
    /// Read the container: the video track, its parameter sets, its samples
    /// and the audio beside it.
    ///
    /// # Errors
    ///
    /// No video track, no `avcC` record, a NAL length prefix ISO/IEC 14496-15
    /// reserves, or a track no sample could be read from.
    pub fn read(mp4_data: &[u8]) -> Result<Self> {
        log::debug!("demux: parsing MP4 data ({} bytes)", mp4_data.len());

        let cursor = Cursor::new(mp4_data);
        let mp4 = Mp4Reader::read_header(cursor, mp4_data.len() as u64)
            .context("Failed to read MP4 header")?;

        log::trace!("MP4 contains {} tracks:", mp4.tracks().len());
        for (id, track) in mp4.tracks() {
            let track_type = track
                .track_type()
                .map(|t| format!("{:?}", t))
                .unwrap_or_else(|_| "Unknown".to_string());
            log::trace!(
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

        let video_track = mp4
            .tracks()
            .values()
            .find(|t| matches!(t.track_type(), Ok(TrackType::Video)))
            .ok_or_else(|| anyhow!("No video track found in MP4"))?;

        let track_id = video_track.track_id();
        let duration = video_track.duration();
        let sample_count = video_track.sample_count();

        // Guarded so an empty track cannot produce fps=0 and hand
        // `Duration::from_secs_f64` an infinity.
        let fps = if sample_count > 0 && duration.as_secs_f64() > 0.0 {
            sample_count as f64 / duration.as_secs_f64()
        } else {
            30.0
        };
        let frame_duration = Duration::from_secs_f64(1.0 / fps);

        // AVCC allows 1-, 2- or 4-byte NAL length prefixes; assuming 4
        // misparses any valid file that uses a narrower one. The 3-byte
        // encoding (length_size_minus_one == 2) is reserved by ISO/IEC
        // 14496-15.
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
            Some(other) => return Err(anyhow!("Unsupported avcC length_size_minus_one: {other}")),
            None => return Err(anyhow!("Video track has no avcC configuration record")),
        };

        let width = u32::from(video_track.width());
        let height = u32::from(video_track.height());

        // Bound the frame before anything is read from the file at all: a
        // malformed or hostile declaration must not be able to ask for a
        // buffer that overflows the multiply or kills the process, and a
        // check made after the samples and the audio have been decoded is a
        // check made after the cost. Both targets, because both allocate
        // from it eventually and neither has a use for a track this shape.
        let pixel_count = (width as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| anyhow!("Video dimensions overflow: {width}x{height}"))?;
        if pixel_count == 0 || pixel_count > MAX_VIDEO_PIXELS {
            return Err(anyhow!(
                "Video dimensions out of range: {width}x{height} ({pixel_count} pixels)"
            ));
        }

        // The coded frame is the sensor's orientation; the track matrix is the
        // correction a phone writes instead of re-encoding.
        let matrix = &video_track.trak.tkhd.matrix;
        let rotation = Rotation::from_matrix(matrix.a, matrix.b, matrix.c, matrix.d);

        log::debug!(
            "Video track {}: {}x{}, {} samples, {:.2} fps, duration: {:.2}s",
            track_id,
            width,
            height,
            sample_count,
            fps,
            duration.as_secs_f64(),
        );
        // Without the bitrate the line used to carry: `Mp4Track::bitrate`
        // multiplies `stsz`'s fixed sample size by its declared count and
        // then by eight, and both of those are numbers a file chose. Near
        // `u32::MAX` each that product overflows `u64`, which is a panic in a
        // debug build and an abort on a page — reached *before* the bounds
        // below, and for a diagnostic nothing depends on.
        log::debug!(
            "Video track details: timescale={}, rotation={:?}",
            video_track.timescale(),
            rotation,
        );

        let sps = video_track
            .sequence_parameter_set()
            .ok()
            .map(<[u8]>::to_vec);
        let pps = video_track.picture_parameter_set().ok().map(<[u8]>::to_vec);
        log::debug!(
            "SPS: {} bytes, PPS: {} bytes",
            sps.as_ref().map_or(0, Vec::len),
            pps.as_ref().map_or(0, Vec::len),
        );
        if let Some(ref sps_data) = sps {
            log_profile(sps_data);
        }

        let sps_pps = build_sps_pps_annexb(sps.as_deref(), pps.as_deref());
        log::debug!("Built SPS/PPS Annex B data: {} bytes", sps_pps.len());

        let samples = extract_samples(mp4_data, track_id, sample_count, nal_length_size)?;
        let audio = super::audio::extract_audio_from_mp4(mp4_data);

        Ok(Self {
            samples,
            sps_pps,
            rotation,
            frame_duration,
            duration,
            width,
            height,
            audio,
        })
    }

    /// How many frames the track turned out to have, which is not what the
    /// container declared: see [`MAX_TRACK_SAMPLES`].
    pub fn frame_count(&self) -> usize {
        self.samples.len()
    }

    /// The frame a position in the video names, clamped to the track.
    pub fn index_at(&self, time: Duration) -> usize {
        let index = (time.as_secs_f64() / self.frame_duration.as_secs_f64()) as usize;
        index.min(self.samples.len().saturating_sub(1))
    }

    /// Where in the video frame `index` sits.
    ///
    /// Computed from the index rather than carried on the picture, because a
    /// WebCodecs timestamp is a label and [`stamp_of`] deliberately makes it
    /// one.
    pub fn timestamp_of(&self, index: usize) -> Duration {
        self.frame_duration.mul_f64(index as f64)
    }

    /// The sample a decode may be entered at to reach `index`.
    pub fn keyframe_at_or_before(&self, index: usize) -> usize {
        keyframe_at_or_before(&self.samples, index)
    }
}

/// What the profile bytes of an SPS say, for a video that will not play.
///
/// openh264 is the one that minds — a browser answers `isConfigSupported` for
/// itself — but the line costs nothing on either target, and a `High 4:4:4`
/// file is exactly the report that arrives with no other clue in it.
fn log_profile(sps_data: &[u8]) {
    if sps_data.is_empty() {
        return;
    }

    let preview: Vec<String> = sps_data
        .iter()
        .take(16)
        .map(|b| format!("{:02x}", b))
        .collect();
    log::debug!("SPS data (first 16 bytes): {}", preview.join(" "));

    // The SPS NAL type is 7, so the first byte is the NAL header and the
    // profile, its constraints and the level follow it.
    if sps_data.len() < 4 {
        return;
    }
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

    log::trace!(
        "H.264 Profile: {} (profile_idc={}), Level: {}.{}, Constraints: 0x{:02x}",
        profile_name,
        profile_idc,
        level_idc / 10,
        level_idc % 10,
        constraint_flags
    );

    if profile_idc >= 100 {
        log::warn!(
            "Video uses {} profile - OpenH264 may have limited support for advanced features",
            profile_name
        );
    }
}

/// Every sample of the video track, rewritten as Annex B.
///
/// The count is the container's, and the container is a file somebody sent:
/// `stsz` can declare a fixed sample size and a count of four billion in a few
/// hundred bytes, which reserves tens of gigabytes before a single sample is
/// read. On a page that is an abort rather than an error, and on the desktop
/// it is the machine's memory, so it is bounded against the only thing that
/// cannot be forged — how many bytes the file actually has, given that a
/// sample carries a length prefix and at least one byte after it.
///
/// Two ceilings, because neither alone is one. The file's length bounds what
/// it can carry, but a fixed one-byte sample size makes that bound tens of
/// millions, and the per-sample bookkeeping is what costs rather than the
/// payload.
fn extract_samples(
    mp4_data: &[u8],
    track_id: u32,
    sample_count: u32,
    nal_length_size: usize,
) -> Result<Vec<H264Sample>> {
    let cursor = Cursor::new(mp4_data);
    let mut mp4 = Mp4Reader::read_header(cursor, mp4_data.len() as u64)?;

    let ceiling = (mp4_data.len() / (nal_length_size + 1)).min(MAX_TRACK_SAMPLES);
    let sample_count = (sample_count as usize).min(ceiling);
    if sample_count == 0 {
        return Err(anyhow!("No video samples could be extracted"));
    }

    // Fallibly, as the second half of the same guard: the bound above is
    // arithmetic on a length, and a very large file would still be asking for
    // a very large reservation.
    let mut samples: Vec<H264Sample> = Vec::new();
    samples
        .try_reserve(sample_count)
        .map_err(|e| anyhow!("no room for {sample_count} video samples: {e}"))?;

    let mut keyframe_count = 0usize;
    let mut total_size = 0usize;
    let mut failed_reads = 0usize;

    for index in 1..=sample_count as u32 {
        match mp4.read_sample(track_id, index) {
            Ok(Some(sample)) => {
                if index == 1 {
                    let preview: Vec<String> = sample
                        .bytes
                        .iter()
                        .take(32)
                        .map(|b| format!("{:02x}", b))
                        .collect();
                    log::debug!("First sample raw data (32 bytes): {}", preview.join(" "));
                    log::debug!("First sample size: {} bytes", sample.bytes.len());
                }

                let data = avcc_to_annexb(&sample.bytes, nal_length_size);
                if index == 1 {
                    log::trace!("First sample NAL types: {:?}", nal_types(&data));
                }

                let is_keyframe = is_keyframe(&data);
                if is_keyframe {
                    keyframe_count += 1;
                }
                total_size += data.len();
                samples.push(H264Sample { data, is_keyframe });
            }
            // Counted always and named only at first. The walk is bounded
            // by the file's length rather than by anything true of the
            // track, so a forged `stsz` reaches this arm for every sample it
            // bought — a million lines in the journal, or in a page's
            // console, for one attachment. The count below is the report.
            Ok(None) => {
                failed_reads += 1;
                if failed_reads <= NAMED_FAILED_READS {
                    log::warn!("sample {index} returned nothing");
                }
            }
            Err(e) => {
                failed_reads += 1;
                if failed_reads <= NAMED_FAILED_READS {
                    log::warn!("could not read sample {index}: {e}");
                }
            }
        }
    }

    log::debug!(
        "Extracted {} samples: {} keyframes, {} failed reads, total size: {} KB",
        samples.len(),
        keyframe_count,
        failed_reads,
        total_size / 1024
    );

    if samples.is_empty() {
        return Err(anyhow!("No video samples could be extracted"));
    }
    if keyframe_count == 0 {
        log::warn!("No keyframes detected! Video may not decode correctly.");
    }

    Ok(samples)
}

/// How many unreadable samples are named individually before the walk stops
/// saying which.
const NAMED_FAILED_READS: usize = 8;

/// Every NAL unit type in `annexb_data`, in order.
///
/// For the two places a decode has already gone wrong, or is about to: what
/// the decoder was handed when it refused a sample, and what the first sample
/// of a file turned out to be. Nothing on the playing path calls it — it walks
/// the buffer a byte at a time and allocates.
pub(super) fn nal_types(annexb_data: &[u8]) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A length prefix is four bytes a file chose, and on the 32-bit target a
    /// large one plus the position is a sum `usize` cannot hold. What that
    /// cost is not a wrong answer but a panic in the slice, on a video
    /// somebody sent.
    #[test]
    fn a_length_that_cannot_be_added_stops_the_walk() {
        let mut sample = vec![0xff, 0xff, 0xff, 0xff];
        sample.extend_from_slice(&[0x65, 0x00]);
        assert!(
            avcc_to_annexb(&sample, 4).is_empty(),
            "a length past the buffer yields nothing rather than panicking"
        );

        // And the units before it are still delivered.
        let mut mixed = vec![0x00, 0x00, 0x00, 0x02, 0x65, 0x88];
        mixed.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        assert_eq!(
            avcc_to_annexb(&mixed, 4),
            [NAL_START_CODE, &[0x65, 0x88]].concat(),
            "what was readable before the bad length still comes back"
        );
    }

    /// The prefix width is the container's, and a narrower one is not an
    /// unusual file: reading every sample as 4-byte-prefixed turns a valid
    /// clip into noise.
    #[test]
    fn a_narrow_length_prefix_is_read_as_written() {
        // Two units of two bytes each, with one-byte lengths.
        let avcc = [0x02, 0x65, 0xAA, 0x02, 0x68, 0xBB];
        assert_eq!(
            avcc_to_annexb(&avcc, 1),
            [0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x68, 0xBB]
        );
    }

    /// A length running past the buffer is a truncated sample, and what came
    /// before it still decodes.
    #[test]
    fn a_truncated_sample_keeps_what_it_had() {
        let avcc = [0x00, 0x00, 0x00, 0x02, 0x65, 0xAA, 0x00, 0x00, 0x00, 0x40];
        assert_eq!(avcc_to_annexb(&avcc, 4), [0, 0, 0, 1, 0x65, 0xAA]);
    }

    /// An IDR is what a decode may be entered at, so recognising one is what
    /// decides where a seek restarts.
    #[test]
    fn an_idr_is_what_makes_a_sample_a_keyframe() {
        assert!(is_keyframe(&[0, 0, 0, 1, 0x65, 0x88, 0x00]));
        // Type 1 is a non-IDR slice.
        assert!(!is_keyframe(&[0, 0, 0, 1, 0x41, 0x9A, 0x00]));
    }

    /// Either set may be absent, and the preamble is still whatever there was.
    #[test]
    fn a_preamble_carries_only_the_sets_that_exist() {
        assert_eq!(
            build_sps_pps_annexb(Some(&[0x67, 0x42]), Some(&[0x68, 0xEE])),
            [0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xEE]
        );
        assert_eq!(build_sps_pps_annexb(None, None), Vec::<u8>::new());
    }
}

#[cfg(test)]
mod seek_tests {
    use super::*;

    fn samples(keyframes: &[bool]) -> Vec<H264Sample> {
        keyframes
            .iter()
            .map(|&is_keyframe| H264Sample {
                data: Vec::new(),
                is_keyframe,
            })
            .collect()
    }

    /// A backward seek re-enters at a keyframe, because entering anywhere
    /// else produces nothing until the next IDR.
    #[test]
    fn a_backward_seek_re_enters_at_a_keyframe() {
        let samples = samples(&[true, false, false, true, false, false]);
        assert_eq!(keyframe_at_or_before(&samples, 5), 3);
        assert_eq!(keyframe_at_or_before(&samples, 3), 3);
        assert_eq!(keyframe_at_or_before(&samples, 2), 0);
    }

    /// A stream with no keyframe at all still has to start somewhere.
    #[test]
    fn a_stream_with_no_keyframe_starts_at_the_beginning() {
        assert_eq!(
            keyframe_at_or_before(&samples(&[false, false, false]), 2),
            0
        );
    }

    /// The stamp is what tells one decoded picture from the next, so it has
    /// to stay unique for the whole track. Microseconds in an `i32` do not:
    /// they run out around thirty-six minutes and every later frame would
    /// carry the same one.
    #[test]
    fn every_sample_of_a_long_video_has_its_own_stamp() {
        assert_eq!(stamp_of(0), 0);
        assert_eq!(stamp_of(1), 1);
        // Half an hour at 30fps, where a microsecond stamp would already be
        // within sight of its ceiling.
        assert_eq!(stamp_of(54_000), 54_000);
        assert_ne!(stamp_of(54_000), stamp_of(54_001));
    }
}

/// The bound on what a container is allowed to make this reserve.
///
/// Its own module because building the fixture takes a writer and a patched
/// `stsz`, and none of that is wanted by the byte-shuffling tests above.
#[cfg(test)]
mod sample_count_tests {
    use super::*;

    /// A tiny but valid MP4 with one AVC track and `count` samples in it.
    fn one_track_mp4(count: u32) -> Vec<u8> {
        use mp4::{AvcConfig, Bytes, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig};

        let mut writer = Mp4Writer::write_start(
            Cursor::new(Vec::new()),
            &Mp4Config {
                major_brand: (*b"isom").into(),
                minor_version: 512,
                compatible_brands: vec![(*b"isom").into(), (*b"avc1").into()],
                timescale: 1000,
            },
        )
        .expect("a writer");

        writer
            .add_track(&TrackConfig::from(MediaConfig::AvcConfig(AvcConfig {
                width: 16,
                height: 16,
                // Not real parameter sets: nothing here decodes, and no real
                // capture may be checked in.
                seq_param_set: vec![0x67, 0x42, 0x00, 0x0a],
                pic_param_set: vec![0x68, 0xce, 0x38, 0x80],
            })))
            .expect("a track");

        for index in 0..count {
            writer
                .write_sample(
                    1,
                    &Mp4Sample {
                        start_time: u64::from(index) * 40,
                        duration: 40,
                        rendering_offset: 0,
                        is_sync: index == 0,
                        // One AVCC unit: a four-byte length and two bytes of
                        // payload, the first of which is an IDR NAL header.
                        bytes: Bytes::from_static(&[0, 0, 0, 2, 0x65, 0x88]),
                    },
                )
                .expect("a sample");
        }
        writer.write_end().expect("an end");
        writer.into_writer().into_inner()
    }

    /// Rewrite the `stsz` box so the track declares `count` samples of a
    /// fixed size.
    ///
    /// A fixed size is the shape that matters: with `sample_size == 0` the
    /// reader checks the count against the box's own length, and with a
    /// non-zero one there is nothing left to check it against — which is
    /// exactly how a few hundred bytes come to declare four billion samples.
    fn declare_samples(mp4_data: &mut [u8], count: u32) {
        let at = mp4_data
            .windows(4)
            .position(|w| w == b"stsz")
            .expect("the fixture has an stsz");
        // `stsz`, then the version and flags, then the fixed sample size and
        // the count.
        let sample_size = at + 4 + 4;
        mp4_data[sample_size..sample_size + 4].copy_from_slice(&6u32.to_be_bytes());
        mp4_data[sample_size + 4..sample_size + 8].copy_from_slice(&count.to_be_bytes());
    }

    /// The fixture has to be a file this reader actually accepts, or the test
    /// below would pass on any error at all.
    #[test]
    fn the_fixture_reads_as_the_track_it_declares() {
        let mp4_data = one_track_mp4(4);
        let track = Track::read(&mp4_data).expect("a readable track");
        assert_eq!(track.frame_count(), 4);
    }

    /// `stsz` is a number a file chose, and the desktop used to reserve from
    /// it directly: `Vec::with_capacity(4_000_000_000)` is tens of gigabytes
    /// asked for before a byte of the track is read, from a file a few
    /// hundred bytes long.
    ///
    /// What bounds it is the one thing the file cannot lie about — how many
    /// bytes it has — and, above that, [`MAX_TRACK_SAMPLES`].
    #[test]
    fn an_absurd_sample_count_is_refused_rather_than_reserved() {
        let mut mp4_data = one_track_mp4(4);
        declare_samples(&mut mp4_data, 4_000_000_000);

        let track = Track::read(&mp4_data).expect("the file is still readable");
        assert!(
            track.frame_count() <= mp4_data.len(),
            "a declaration cannot buy more samples than the file has bytes: \
             {} samples out of {} bytes",
            track.frame_count(),
            mp4_data.len()
        );
        assert!(track.frame_count() <= MAX_TRACK_SAMPLES);
    }

    /// And the same bound holds when the count is merely large rather than
    /// absurd: a file inside the page's media budget can still name tens of
    /// millions of samples with a one-byte fixed size, and the bookkeeping is
    /// what costs.
    #[test]
    fn a_count_past_the_ceiling_is_cut_to_it() {
        let mut mp4_data = one_track_mp4(4);
        declare_samples(&mut mp4_data, u32::MAX);

        let track = Track::read(&mp4_data).expect("the file is still readable");
        assert!(track.frame_count() <= mp4_data.len() / 5);
    }

    /// A track declaring nothing is an error rather than an empty player.
    #[test]
    fn a_track_declaring_no_samples_is_an_error() {
        let mut mp4_data = one_track_mp4(4);
        declare_samples(&mut mp4_data, 0);
        assert!(Track::read(&mp4_data).is_err());
    }
}
