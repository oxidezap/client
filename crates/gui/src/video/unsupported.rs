//! A video attachment, decoded by the browser.
//!
//! The same job as [`super::streaming`] and half the code, because only the
//! decoder differs: `mp4` demuxes the container on both targets, and what
//! openh264 does there [`super::webcodecs`] does here.
//!
//! # Why the shape survives
//!
//! The player pulls by index — `seek_to_frame`, then `current_frame` — and
//! `VideoDecoder` pushes. Bridging that without changing every caller rests
//! on one fact about how this is drawn: playback is a timer asking for the
//! frame it is about to paint, many times a second. So a seek feeds the
//! decoder and returns, and the picture appears on a later ask rather than
//! this one. What that costs is a frame of latency at the start of a play or
//! after a scrub; what it saves is `VideoPlayer` and every bubble above it.
//!
//! Frames still arrive in decode order and are held one at a time, so this is
//! *not* a general seek: entering the stream anywhere but a keyframe produces
//! nothing until the next IDR. That is why a backward seek resets and replays
//! from the keyframe before the target, which is what the native decoder does
//! for the same reason.
//!
//! # When it does not work
//!
//! Every failure lands where this module used to land unconditionally: an
//! error out of `new`, which the player already draws as "this video cannot
//! be played here", keeping the thumbnail. A browser with no WebCodecs, a
//! codec it will not configure, a file with no readable parameter set — all
//! of them arrive there, so the path that existed before is still the floor.

use std::io::Cursor;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use gpui::RenderImage;
use mp4::{Mp4Reader, TrackType};

use super::audio::VideoAudio;
use super::demux::{
    H264Sample, avcc_to_annexb, build_sps_pps_annexb, is_keyframe, keyframe_at_or_before,
    stamp_micros,
};
use super::geometry::Rotation;
use super::webcodecs;

/// One decoded frame, in the shape the player above expects.
pub struct StreamingFrame {
    pub image: std::sync::Arc<RenderImage>,
    pub timestamp: Duration,
    pub index: usize,
}

/// An attachment's samples, and the browser decoder reading them.
pub struct StreamingVideoDecoder {
    samples: Vec<H264Sample>,
    /// Prepended to the first unit fed after every reset, because a browser
    /// decoder configured without a `description` learns the geometry from
    /// the stream and a reset forgets it.
    sps_pps: Vec<u8>,
    frame_duration: Duration,
    duration: Duration,
    decoder: webcodecs::Decoder,
    /// The furthest sample handed to the decoder, so a forward seek continues
    /// rather than replaying.
    last_fed_index: i32,
    /// What index the picture in the decoder's slot is for.
    ///
    /// Tracked here rather than read back, because a decoded frame carries
    /// the timestamp we stamped on it and nothing else: the mapping from
    /// timestamp to index is this side's.
    awaiting_index: Option<usize>,
    current_frame: Option<StreamingFrame>,
    audio: Option<VideoAudio>,
}

impl StreamingVideoDecoder {
    /// Demux the container and configure the browser's decoder for it.
    ///
    /// # Errors
    ///
    /// No video track, a container this build cannot read, or a browser that
    /// will not decode the stream.
    pub fn new(mp4_data: &[u8]) -> Result<Self> {
        let cursor = Cursor::new(mp4_data);
        let mp4 = Mp4Reader::read_header(cursor, mp4_data.len() as u64)
            .context("Failed to read MP4 header")?;

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

        // AVCC allows 1-, 2- or 4-byte NAL length prefixes; 3 is reserved.
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

        let avcc = video_track
            .trak
            .mdia
            .minf
            .stbl
            .stsd
            .avc1
            .as_ref()
            .map(|avc1| &avc1.avcc);
        let sps_pps = build_sps_pps_annexb(
            avcc.and_then(|a| a.sequence_parameter_sets.first())
                .map(|s| s.bytes.as_slice()),
            avcc.and_then(|a| a.picture_parameter_sets.first())
                .map(|p| p.bytes.as_slice()),
        );

        // The display matrix, for the same reason the native path reads it: a
        // phone records in its sensor's orientation and writes the correction
        // here, so only this says which way is up.
        let matrix = &video_track.trak.tkhd.matrix;
        let rotation = Rotation::from_matrix(matrix.a, matrix.b, matrix.c, matrix.d);

        let samples = Self::extract_samples(mp4_data, track_id, sample_count, nal_length_size)?;
        let decoder = webcodecs::Decoder::new(&sps_pps, rotation).map_err(|e| anyhow!(e))?;
        let audio = super::audio::extract_audio_from_mp4(mp4_data);

        Ok(Self {
            samples,
            sps_pps,
            frame_duration,
            duration,
            decoder,
            last_fed_index: -1,
            awaiting_index: None,
            current_frame: None,
            audio,
        })
    }

    /// Every sample of the video track, rewritten as Annex B.
    fn extract_samples(
        mp4_data: &[u8],
        track_id: u32,
        sample_count: u32,
        nal_length_size: usize,
    ) -> Result<Vec<H264Sample>> {
        let cursor = Cursor::new(mp4_data);
        let mut mp4 = Mp4Reader::read_header(cursor, mp4_data.len() as u64)?;

        let mut samples = Vec::with_capacity(sample_count as usize);
        for index in 1..=sample_count {
            match mp4.read_sample(track_id, index) {
                Ok(Some(sample)) => {
                    let data = avcc_to_annexb(&sample.bytes, nal_length_size);
                    let is_keyframe = is_keyframe(&data);
                    samples.push(H264Sample { data, is_keyframe });
                }
                Ok(None) => log::warn!("sample {index} returned nothing"),
                Err(e) => log::warn!("could not read sample {index}: {e}"),
            }
        }

        if samples.is_empty() {
            return Err(anyhow!("No video samples could be extracted"));
        }
        Ok(samples)
    }

    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn seek(&mut self, time: Duration) {
        let target = (time.as_secs_f64() / self.frame_duration.as_secs_f64()) as usize;
        self.seek_to_frame(target.min(self.samples.len().saturating_sub(1)));
    }

    /// Feed the decoder up to `target_index`, and collect whatever has landed.
    ///
    /// Returns having *asked* rather than having answered: the picture comes
    /// back on the browser's callback, so [`Self::current_frame`] is what
    /// eventually sees it. See the module note on why that is enough.
    pub fn seek_to_frame(&mut self, target_index: usize) {
        if target_index >= self.samples.len() {
            return;
        }
        if self
            .current_frame
            .as_ref()
            .is_some_and(|frame| frame.index == target_index)
        {
            self.collect();
            return;
        }

        let start = if target_index as i32 > self.last_fed_index {
            (self.last_fed_index + 1) as usize
        } else {
            // Backwards: the decoder's reference chain only runs forwards, so
            // the stream is re-entered at the keyframe at or before the
            // target and replayed to it.
            self.decoder.reset();
            self.last_fed_index = -1;
            self.awaiting_index = None;
            keyframe_at_or_before(&self.samples, target_index)
        };

        for index in start..=target_index {
            let Some(sample) = self.samples.get(index) else {
                break;
            };
            // The parameter sets ride with the first unit after a reset: a
            // decoder configured without a description takes its geometry
            // from the stream, and a reset leaves it with none.
            let unit = if index == start && start > 0 || (index == 0 && self.last_fed_index < 0) {
                let mut with_sets = self.sps_pps.clone();
                with_sets.extend_from_slice(&sample.data);
                with_sets
            } else {
                sample.data.clone()
            };
            self.decoder.decode(
                &unit,
                stamp_micros(index, self.frame_duration),
                sample.is_keyframe,
            );
            self.last_fed_index = index as i32;
        }
        self.awaiting_index = Some(target_index);
        self.collect();
    }

    /// Take whatever the decoder has produced since the last look.
    fn collect(&mut self) {
        let Some(picture) = self.decoder.newest() else {
            return;
        };
        let index = self.awaiting_index.unwrap_or(0);
        let already = self
            .current_frame
            .as_ref()
            .is_some_and(|frame| frame.index == index);
        if already {
            return;
        }
        self.current_frame = Some(StreamingFrame {
            image: picture.image,
            timestamp: Duration::from_micros(picture.timestamp_micros.max(0) as u64),
            index,
        });
    }

    /// The newest decoded frame, if one has arrived yet.
    #[must_use]
    pub fn current_frame(&self) -> Option<&StreamingFrame> {
        self.current_frame.as_ref()
    }

    pub fn reset(&mut self) {
        self.decoder.reset();
        self.last_fed_index = -1;
        self.awaiting_index = None;
        self.current_frame = None;
    }

    pub fn take_audio(&mut self) -> Option<VideoAudio> {
        self.audio.take()
    }
}
