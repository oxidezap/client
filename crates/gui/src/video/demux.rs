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

/// The stamp a sample is fed under, which is its rank in *presentation* order.
///
/// A WebCodecs timestamp is a label rather than a clock — nothing but this
/// side reads it — so a rank is the one value that stays unique for the whole
/// track. Microseconds would not: the binding takes an `i32`, which runs out
/// around thirty-six minutes, and every frame past that would carry the same
/// stamp. A reader keying on the stamp to tell one picture from the next then
/// sees them all as the same picture and freezes on the first, while playback
/// goes on advancing.
///
/// The rank rather than the decode index, and that is the whole of the
/// B-frame fix: a browser answers in presentation order and hands the label
/// back with the picture, so labelling a sample with where it was *fed* makes
/// the answers arrive out of sequence and the timeline read a picture as a
/// position it does not hold. The two agree exactly while decode order is
/// presentation order, which is every baseline stream and so every video
/// WhatsApp itself sends — which is why this was invisible until an
/// attachment carried B-frames. [`Timeline`] is what keeps the two apart.
///
/// The displayed position is computed from the rank instead; see
/// `StreamingFrame::timestamp`.
// Read by the browser's decoder and by the tests below; a native build
// compiles neither caller.
#[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
pub(super) fn stamp_of(rank: usize) -> i32 {
    i32::try_from(rank).unwrap_or(i32::MAX)
}

/// Which sample is shown when, kept apart from the order they are decoded in.
///
/// Two orders, and the container carries both: `stts` says when a sample is
/// *decoded* and `ctts` carries the offset from that to when it is *shown*.
/// They differ exactly when a stream has B-frames, because a picture that
/// references a later one has to be decoded after it and displayed before it.
///
/// So everything above the demux — a seek, a position on the scrubber, the
/// index on a decoded picture — counts in *ranks*, which are positions in
/// presentation order, and only the feed loop counts in decode indices. A
/// seek is then "the decode samples this presentation position depends on"
/// rather than a range.
pub(super) struct Timeline {
    /// The decode index of each presentation rank.
    by_rank: Vec<usize>,
    /// The presentation rank of each decode index — [`Self::by_rank`]
    /// inverted, held rather than searched because the feed loop asks once
    /// per sample it hands over.
    rank_of: Vec<usize>,
    /// The furthest sample any picture up to each rank is coded as — a
    /// running maximum over [`Self::by_rank`], and what a feed loop actually
    /// walks to.
    ///
    /// Not [`Self::by_rank`] itself, because that is *not monotonic*: an
    /// IBBP run maps ranks to decode indices 0, 2, 3, 1, 5, 6, 4, so playing
    /// forward would ask for a sample behind the cursor about every third
    /// frame, and every one of those reads as a backward seek — a decoder
    /// reset and a replay from the last keyframe, several times a second, on
    /// exactly the streams this ordering exists for.
    through: Vec<usize>,
    /// When each rank is shown, from the container's own composition times.
    at: Vec<Duration>,
}

impl Timeline {
    /// Order `composition` — one composition time per sample, in decode order
    /// and in the track's timescale — into the order they are shown in.
    ///
    /// `fallback` is the uniform spacing to fall back to when the container
    /// says nothing usable about time — see the guard below, which is about
    /// more files than a missing timescale.
    fn of(composition: &[i64], timescale: u32, fallback: Duration) -> Self {
        let mut by_rank: Vec<usize> = (0..composition.len()).collect();
        // Stable, so two samples sharing a composition time — a broken file
        // rather than a choice — keep their decode order instead of being
        // ordered differently on different runs for no reason anybody could
        // find.
        by_rank.sort_by_key(|&index| composition[index]);

        let mut rank_of = vec![0usize; composition.len()];
        let mut through = Vec::with_capacity(by_rank.len());
        let mut furthest = 0usize;
        for (rank, &index) in by_rank.iter().enumerate() {
            rank_of[index] = rank;
            furthest = furthest.max(index);
            through.push(furthest);
        }

        // Relative to the first picture shown rather than to zero: a track
        // may start at any composition time, and what a player needs is the
        // offset into the film.
        let origin = by_rank.first().map_or(0, |&index| composition[index]);
        // Whether the file said anything about *when*, rather than whether it
        // declared a timescale. A fragmented track with no default sample
        // duration reads back a start time of zero for every sample, which is
        // a perfectly good timescale over times that never advance — and a
        // timeline of all zeroes answers every position with the last
        // picture, so a video would open on its final frame.
        let told = timescale != 0
            && by_rank
                .last()
                .zip(by_rank.first())
                .is_some_and(|(last, first)| composition[*last] > composition[*first]);
        let at = by_rank
            .iter()
            .enumerate()
            .map(|(rank, &index)| {
                if !told {
                    return fallback.mul_f64(rank as f64);
                }
                let ticks = composition[index].saturating_sub(origin).max(0);
                Duration::from_secs_f64(ticks as f64 / f64::from(timescale))
            })
            .collect();

        Self {
            by_rank,
            rank_of,
            through,
            at,
        }
    }

    /// The sample that has to be decoded for the picture at `rank`.
    fn decode_index(&self, rank: usize) -> usize {
        self.by_rank.get(rank).copied().unwrap_or(0)
    }

    /// The sample a decode has to reach for the picture at `rank` to have
    /// been produced — see [`Self::through`].
    fn decode_through(&self, rank: usize) -> usize {
        self.through.get(rank).copied().unwrap_or(0)
    }

    /// Where the sample at `decode_index` is shown.
    // As `stamp_of`: only the browser's feed loop stamps a unit with this,
    // and a native build compiles no caller of it outside the tests.
    #[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
    fn rank(&self, decode_index: usize) -> usize {
        self.rank_of.get(decode_index).copied().unwrap_or(0)
    }

    /// When the picture at `rank` is shown.
    fn time(&self, rank: usize) -> Duration {
        self.at.get(rank).copied().unwrap_or_default()
    }

    /// The last picture shown at or before `time`.
    ///
    /// A search rather than a division, because composition times are the
    /// container's and a track is not obliged to space them evenly.
    fn rank_at(&self, time: Duration) -> usize {
        match self.at.binary_search(&time) {
            Ok(rank) => rank,
            Err(0) => 0,
            Err(next) => next - 1,
        }
    }
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
    /// Every sample of the video track, Annex B, still compressed, in the
    /// order they are *decoded* in. [`Self::timeline`] is what says when each
    /// of them is shown.
    pub samples: Vec<H264Sample>,
    /// Which sample is shown when. Every index above the demux is a rank in
    /// here; the samples are indexed in decode order and only the feed loops
    /// walk them that way.
    timeline: Timeline,
    /// The parameter sets as an Annex B preamble: what a decoder is
    /// configured with, and what rides with the first unit after a reset.
    pub sps_pps: Vec<u8>,
    /// Display rotation from the track matrix, applied to every kept frame.
    pub rotation: Rotation,
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

        let Demuxed {
            samples,
            composition,
        } = extract_samples(mp4_data, track_id, sample_count, nal_length_size)?;
        // The container's own answer to "when is this shown", which is what
        // makes a B-frame stream place its pictures where they belong. A
        // timescale of zero is a file that declared nothing usable, and the
        // uniform spacing below is the honest fallback.
        let timeline = Timeline::of(&composition, video_track.timescale(), frame_duration);
        let audio = super::audio::extract_audio_from_mp4(mp4_data);

        Ok(Self {
            samples,
            timeline,
            sps_pps,
            rotation,
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

    /// The picture a position in the video names, clamped to the track.
    ///
    /// A rank, like every other index a caller of this type holds.
    pub fn index_at(&self, time: Duration) -> usize {
        self.timeline
            .rank_at(time)
            .min(self.samples.len().saturating_sub(1))
    }

    /// Where in the video the picture at `rank` sits.
    ///
    /// The container's own composition time, which is the whole point of the
    /// rank: a WebCodecs timestamp is a label and [`stamp_of`] deliberately
    /// makes it one, so the position cannot be read back off the picture.
    pub fn timestamp_of(&self, rank: usize) -> Duration {
        self.timeline.time(rank)
    }

    /// The sample the picture at `rank` is coded as.
    ///
    /// One of the two places the orders meet. What a feed loop walks *to* is
    /// [`Self::decode_through`]; this is what the picture itself is, and what
    /// a re-entry point is measured from.
    pub fn decode_index_of(&self, rank: usize) -> usize {
        self.timeline.decode_index(rank)
    }

    /// The sample a decode has to reach before the picture at `rank` has been
    /// produced.
    ///
    /// At or after [`Self::decode_index_of`], and — unlike it — never going
    /// backwards as the rank advances, which is what makes ordinary playback
    /// a walk forwards rather than a reset every third frame. A decoder that
    /// reorders has produced the picture by the time it has been fed this
    /// far, and says which one it is by the stamp it hands back.
    pub fn decode_through(&self, rank: usize) -> usize {
        self.timeline.decode_through(rank)
    }

    /// Where the sample at `decode_index` is shown — the stamp it is fed
    /// under, and the index its picture comes back as.
    // As `stamp_of`: the browser's decoder is the only caller, because the
    // desktop's knows the rank it asked for and is never handed one back.
    #[cfg_attr(not(target_family = "wasm"), allow(dead_code))]
    pub fn rank_of(&self, decode_index: usize) -> usize {
        self.timeline.rank(decode_index)
    }

    /// The sample a decode may be entered at to reach the picture at `rank`.
    ///
    /// In decode order, because that is what re-entering means: every sample
    /// the target references precedes it there, so replaying from the
    /// keyframe before it is what produces the picture.
    pub fn keyframe_for(&self, rank: usize) -> usize {
        keyframe_at_or_before(&self.samples, self.decode_index_of(rank))
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
/// What one walk of the track produces: the units, and when each of them is
/// shown.
///
/// Two vectors of the same length rather than a field on [`H264Sample`],
/// because the composition times are read once into a [`Timeline`] and never
/// looked at again — while the samples are walked by both decoders on every
/// seek.
struct Demuxed {
    samples: Vec<H264Sample>,
    /// One composition time per sample, in decode order and in the track's
    /// timescale: what the container says about *when*, before anything has
    /// been ordered by it.
    composition: Vec<i64>,
}

fn extract_samples(
    mp4_data: &[u8],
    track_id: u32,
    sample_count: u32,
    nal_length_size: usize,
) -> Result<Demuxed> {
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
    let mut composition: Vec<i64> = Vec::new();
    composition
        .try_reserve(sample_count)
        .map_err(|e| anyhow!("no room for {sample_count} sample times: {e}"))?;

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
                // Decode time plus the container's composition offset, which
                // is the one thing that says a B-frame is shown before the
                // picture it references. Widened and saturating because both
                // halves are numbers a file chose: `start_time` is a `u64`
                // and the offset an `i32` that may be negative.
                composition.push(
                    i64::try_from(sample.start_time)
                        .unwrap_or(i64::MAX)
                        .saturating_add(i64::from(sample.rendering_offset)),
                );
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

    Ok(Demuxed {
        samples,
        composition,
    })
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
    ///
    /// Shared with the presentation-order tests below, which need the same
    /// file with only its composition offsets changed — the point being that
    /// everything else about the two is identical.
    pub(super) fn one_track_mp4(count: u32) -> Vec<u8> {
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

/// Decode order and presentation order, and what happens when they differ.
///
/// Its own module for the same reason as the one above: the fixture takes a
/// writer, and a `ctts` box is a shape none of the byte-shuffling tests want.
#[cfg(test)]
mod presentation_order_tests {
    use super::*;

    /// A track whose composition order is not its decode order.
    ///
    /// Four samples in the shape a stream with B-frames has: an IDR, then the
    /// picture the two after it are coded *against* — which therefore has to
    /// be decoded before them and shown after them. The composition offsets
    /// are what the container carries to say so, and are the only thing here
    /// that differs from the baseline fixture above.
    ///
    /// | decoded | dts | offset | shown |
    /// |---------|-----|--------|-------|
    /// | 0 (IDR) |   0 |      0 |     0 |
    /// | 1       |  40 |    120 |   160 |
    /// | 2       |  80 |      0 |    80 |
    /// | 3       | 120 |      0 |   120 |
    ///
    /// Nothing in it decodes: no real capture may be checked in, and every
    /// test here is about the container's bookkeeping rather than about
    /// pictures.
    fn reordered_mp4() -> Vec<u8> {
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
                seq_param_set: vec![0x67, 0x42, 0x00, 0x0a],
                pic_param_set: vec![0x68, 0xce, 0x38, 0x80],
            })))
            .expect("a track");

        for (index, rendering_offset) in [0i32, 120, 0, 0].into_iter().enumerate() {
            let index = index as u64;
            writer
                .write_sample(
                    1,
                    &Mp4Sample {
                        start_time: index * 40,
                        duration: 40,
                        rendering_offset,
                        is_sync: index == 0,
                        // One AVCC unit: a four-byte length and two bytes of
                        // payload. An IDR NAL header on the first and a
                        // non-IDR slice on the rest, so the only sample a
                        // decode may be entered at is the one that should be.
                        bytes: if index == 0 {
                            Bytes::from_static(&[0, 0, 0, 2, 0x65, 0x88])
                        } else {
                            Bytes::from_static(&[0, 0, 0, 2, 0x41, 0x9a])
                        },
                    },
                )
                .expect("a sample");
        }
        writer.write_end().expect("an end");
        writer.into_writer().into_inner()
    }

    /// A picture is placed where the container says it is *shown*, not where
    /// it happened to be decoded.
    ///
    /// The two agree on every baseline stream — and so on every video
    /// WhatsApp itself sends — which is why deriving the position from the
    /// decode index was invisible until an attachment carried B-frames. Here
    /// the second picture shown is the third sample decoded, and reading the
    /// decode index as a position labels it 40 ms when it is shown at 80.
    #[test]
    fn a_picture_sits_where_it_is_shown_rather_than_where_it_was_decoded() {
        let track = Track::read(&reordered_mp4()).expect("a readable track");
        assert_eq!(track.frame_count(), 4);

        assert_eq!(track.timestamp_of(0), Duration::from_millis(0));
        assert_eq!(track.timestamp_of(1), Duration::from_millis(80));
        assert_eq!(track.timestamp_of(2), Duration::from_millis(120));
        assert_eq!(
            track.timestamp_of(3),
            Duration::from_millis(160),
            "the sample decoded second is the last one shown"
        );
    }

    /// And a position asked for in time names that same picture, so a scrub
    /// and the picture it lands on agree.
    #[test]
    fn a_position_in_time_names_the_picture_shown_there() {
        let track = Track::read(&reordered_mp4()).expect("a readable track");

        assert_eq!(track.index_at(Duration::from_millis(0)), 0);
        assert_eq!(track.index_at(Duration::from_millis(80)), 1);
        assert_eq!(track.index_at(Duration::from_millis(119)), 1);
        assert_eq!(track.index_at(Duration::from_millis(120)), 2);
        assert_eq!(track.index_at(Duration::from_millis(160)), 3);
        // Past the end is the last picture, not a panic and not a wrap.
        assert_eq!(track.index_at(Duration::from_secs(9)), 3);
    }

    /// The decode cursor is kept apart from the position, which is the whole
    /// of the fix: a seek is the samples a presentation position depends on,
    /// and the stamp a sample is fed under is where its picture belongs.
    #[test]
    fn a_seek_asks_for_the_samples_a_position_depends_on() {
        let track = Track::read(&reordered_mp4()).expect("a readable track");

        // The last picture shown is coded as the *second* sample, so
        // reaching it means decoding two samples rather than all four.
        assert_eq!(track.decode_index_of(3), 1);
        assert_eq!(track.decode_index_of(1), 2);
        // And back: the sample fed second carries the stamp of the picture
        // shown last, which is what the browser hands back with it.
        assert_eq!(track.rank_of(1), 3);
        assert_eq!(stamp_of(track.rank_of(1)), 3);

        // Every sample maps to its own rank and back, so no two pictures can
        // arrive under one stamp.
        for decode_index in 0..track.frame_count() {
            assert_eq!(
                track.decode_index_of(track.rank_of(decode_index)),
                decode_index
            );
        }

        // Re-entry is in decode order, because that is what a reference chain
        // is: only the first sample is an IDR here.
        assert_eq!(track.keyframe_for(3), 0);
        assert_eq!(track.keyframe_for(1), 0);
    }

    /// Playing forward walks forward. The sample a picture *is* goes
    /// backwards as the ranks advance — that is what reordering means — and
    /// a feed loop that chased it would read every such step as a backward
    /// seek: a decoder reset and a replay from the last keyframe, several
    /// times a second, on exactly the streams this ordering exists for.
    #[test]
    fn playing_forward_never_asks_for_a_sample_behind_the_cursor() {
        let track = Track::read(&reordered_mp4()).expect("a readable track");

        // The sample the picture is coded as does go backwards…
        assert!(track.decode_index_of(3) < track.decode_index_of(2));

        // …and what the feed walks to does not.
        let mut cursor = 0;
        for rank in 0..track.frame_count() {
            let through = track.decode_through(rank);
            assert!(
                through >= cursor,
                "rank {rank} asked the feed to go back to {through} from {cursor}"
            );
            assert!(
                through >= track.decode_index_of(rank),
                "rank {rank} was not decoded by the time the feed reached {through}"
            );
            cursor = through;
        }
        assert_eq!(track.decode_through(3), 3);
    }

    /// A container can declare a perfectly good timescale over times that
    /// never advance — a fragmented track with no default sample duration
    /// reads back a start time of zero for every sample. Placed at those
    /// times, every position in the video answers with the last picture, so
    /// the film opens on its own final frame.
    #[test]
    fn a_track_that_never_says_when_is_spaced_evenly() {
        let timeline = Timeline::of(&[0, 0, 0, 0], 1000, Duration::from_millis(40));

        assert_eq!(timeline.time(0), Duration::ZERO);
        assert_eq!(timeline.time(3), Duration::from_millis(120));
        assert_eq!(timeline.rank_at(Duration::from_millis(80)), 2);
        // And the order is the one order there was.
        assert_eq!(timeline.decode_index(2), 2);
    }

    /// A stream whose orders agree is untouched by any of it: the same
    /// uniform positions, and a rank that is its own decode index.
    #[test]
    fn a_baseline_stream_reads_exactly_as_it_did() {
        let track =
            Track::read(&super::sample_count_tests::one_track_mp4(4)).expect("a readable track");

        for index in 0..4 {
            assert_eq!(track.decode_index_of(index), index);
            assert_eq!(track.rank_of(index), index);
            assert_eq!(
                track.timestamp_of(index),
                Duration::from_millis(40 * index as u64)
            );
            assert_eq!(
                track.index_at(Duration::from_millis(40 * index as u64)),
                index
            );
        }
    }
}
