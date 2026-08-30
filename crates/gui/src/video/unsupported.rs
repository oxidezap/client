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
    H264Sample, MAX_TRACK_SAMPLES, avcc_to_annexb, build_sps_pps_annexb, is_keyframe,
    keyframe_at_or_before, stamp_of,
};
use super::geometry::Rotation;
use super::webcodecs;

/// How many units may sit in the browser's decode queue before this stops
/// feeding it.
///
/// The desktop path bounds its own queue at four frames; this bounds the one
/// on the far side of the binding, which is the only one that exists here. A
/// little deeper than four, because a seek wants a run of pictures decoded
/// and thrown away to reach the one it is after, and each tick resumes where
/// the last stopped.
const MAX_QUEUED_UNITS: u32 = 16;

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
    /// The turn the decoder is built with, kept because it may have to be
    /// built more than once. See [`Self::release`].
    rotation: Rotation,
    /// Absent while nothing is playing.
    ///
    /// A `VideoDecoder` is a hardware codec session, not a buffer, and a
    /// browser allows only a handful at once. The window caches players so a
    /// clip replays without re-fetching, which meant every attachment opened
    /// in a conversation held a session for as long as its player lived: open
    /// a few different clips and the next `Decoder::new` fails, on a page
    /// where nothing is playing at all. What is worth caching is the demuxed
    /// samples, which cost nothing but memory, so the session goes and they
    /// stay.
    decoder: Option<webcodecs::Decoder>,
    /// The furthest sample handed to the decoder, so a forward seek continues
    /// rather than replaying.
    last_fed_index: i32,
    /// Whether the next unit fed has to carry the parameter sets.
    ///
    /// A flag rather than a condition on the index: a decoder configured
    /// without a `description` learns its geometry from the stream, and what
    /// decides whether it still knows it is a *reset*, not where in the
    /// samples the next feed happens to start. Derived from the index, an
    /// ordinary forward step re-sent the sets on every frame.
    needs_parameter_sets: bool,
    /// The stamp of the picture currently held, so an arrival is recognised
    /// as new by *what it is* rather than by what was last asked for.
    ///
    /// A seek feeds several samples and the decoder answers with the first of
    /// them before the target. Labelling whatever is in the slot with the
    /// requested index made that first picture look like the target, and
    /// every later poll then saw the index as already current and returned —
    /// so a scrub stopped on the frame the replay started at.
    shown_stamp: Option<i32>,
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
        Self::attach(Prepared::demux(mp4_data)?)
    }

    /// Build the decoder from work already done.
    ///
    /// Split from [`Self::new`] because only this half touches JS: the demux
    /// and the AAC decode are plain Rust over the whole file, twice, and
    /// running them on the window thread is a conversation that stops
    /// scrolling for as long as a long attachment takes to open. See
    /// [`super::build_decoder`].
    ///
    /// # Errors
    ///
    /// The browser will not decode this stream.
    pub fn attach(prepared: Prepared) -> Result<Self> {
        // Built here rather than lazily, so a stream this browser will not
        // take is refused while somebody is still looking at the press that
        // asked for it.
        let decoder = webcodecs::Decoder::new(&prepared.sps_pps, prepared.rotation)
            .map_err(|e| anyhow!(e))?;
        Ok(Self {
            samples: prepared.samples,
            sps_pps: prepared.sps_pps,
            frame_duration: prepared.frame_duration,
            duration: prepared.duration,
            rotation: prepared.rotation,
            decoder: Some(decoder),
            last_fed_index: -1,
            needs_parameter_sets: true,
            shown_stamp: None,
            current_frame: None,
            audio: prepared.audio,
        })
    }

    /// The decoder, built if this player let its session go.
    ///
    /// `None` only when the browser refuses to build one, which for a stream
    /// that already configured once means it has run out of sessions rather
    /// than that the stream is bad. The caller reports it like any other
    /// decode failure.
    fn decoder(&mut self) -> Option<&webcodecs::Decoder> {
        if self.decoder.is_none() {
            match webcodecs::Decoder::new(&self.sps_pps, self.rotation) {
                Ok(built) => {
                    // A new session knows nothing, so the next unit fed has
                    // to carry the parameter sets and the walk starts again.
                    self.last_fed_index = -1;
                    self.needs_parameter_sets = true;
                    self.shown_stamp = None;
                    self.decoder = Some(built);
                }
                Err(e) => {
                    log::warn!("could not open a decoder for this video again: {e}");
                    return None;
                }
            }
        }
        self.decoder.as_ref()
    }

    /// Give up the codec session, keeping everything that cost a download.
    ///
    /// Called when playback stops. The samples, the parameter sets and the
    /// audio stay, so replaying is a decode rather than a fetch; the session
    /// goes, so a conversation full of clips does not hold one each.
    pub fn release(&mut self) {
        self.decoder = None;
        self.last_fed_index = -1;
        self.needs_parameter_sets = true;
        self.shown_stamp = None;
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

        // The count is the container's, and the container is a file somebody
        // sent: `stsz` can declare a fixed sample size and a count of four
        // billion in a few hundred bytes, which reserves tens of gigabytes
        // before a single sample is read. On this target that is an abort
        // rather than an error, so it is bounded against the only thing that
        // cannot be forged, which is how many bytes the file actually has: a
        // sample carries a length prefix and at least one byte after it.
        // Two ceilings, because neither alone is one. The file's length
        // bounds what it can carry, but a fixed one-byte sample size makes
        // that bound tens of millions, and the per-sample bookkeeping is what
        // costs rather than the payload.
        let ceiling = (mp4_data.len() / (nal_length_size + 1)).min(MAX_TRACK_SAMPLES);
        let sample_count = (sample_count as usize).min(ceiling);
        if sample_count == 0 {
            return Err(anyhow!("No video samples could be extracted"));
        }

        // Fallibly, as the second half of the same guard: the bound above is
        // arithmetic on a length, and a very large file would still be asking
        // for a very large reservation.
        let mut samples: Vec<H264Sample> = Vec::new();
        samples
            .try_reserve(sample_count)
            .map_err(|e| anyhow!("no room for {sample_count} video samples: {e}"))?;
        for index in 1..=sample_count as u32 {
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
}

/// Everything an attachment needs before a decoder is built for it.
///
/// Plain Rust and `Send`, deliberately: this is the expensive half and it can
/// run wherever there is a thread to run it on, while the `VideoDecoder` it
/// feeds exists only on the one that made it.
pub struct Prepared {
    samples: Vec<H264Sample>,
    sps_pps: Vec<u8>,
    rotation: Rotation,
    frame_duration: Duration,
    duration: Duration,
    audio: Option<VideoAudio>,
}

impl Prepared {
    /// Read the container and the audio track.
    ///
    /// # Errors
    ///
    /// No video track, or a container this build cannot read.
    pub fn demux(mp4_data: &[u8]) -> Result<Self> {
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

        let samples = StreamingVideoDecoder::extract_samples(
            mp4_data,
            track_id,
            sample_count,
            nal_length_size,
        )?;
        let audio = super::audio::extract_audio_from_mp4(mp4_data);

        Ok(Self {
            samples,
            sps_pps,
            rotation,
            frame_duration,
            duration,
            audio,
        })
    }
}

impl StreamingVideoDecoder {
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

        // Already fed, and its picture is on the way: the browser decodes on
        // its own schedule, so "asked for and not arrived" is the ordinary
        // state a moment after a seek. Resetting here threw away the very
        // work that was about to answer, and then replayed the whole group of
        // The session may have been given up when playback last stopped, in
        // which case this rebuilds it and resets the walk, so the equality
        // check below is against a fresh `last_fed_index` rather than a
        // remembered one.
        if self.decoder().is_none() {
            return;
        }

        // A picture dropped for want of a copy slot is one this walk already
        // counted as fed. Left alone, the equality branch below then answers
        // "already asked for" on every later poll while the answer is never
        // coming, so a paused scrub stops short of the frame somebody
        // selected. Replaying is what makes it come, and by then the copies
        // that crowded it out have drained.
        let refused = self
            .decoder
            .as_ref()
            .is_some_and(webcodecs::Decoder::take_refusal);

        // pictures to ask for it again.
        if !refused
            && i64::try_from(target_index).unwrap_or(i64::MAX) == i64::from(self.last_fed_index)
        {
            self.collect();
            return;
        }

        // A refusal takes the replay path whatever the target is, because
        // what has to be redone is a picture and not a position: the decoder
        // is re-entered at the keyframe before the target and walked to it,
        // which is exactly what a backward seek already does.
        let start = if !refused && target_index as i32 > self.last_fed_index {
            (self.last_fed_index + 1) as usize
        } else {
            // Backwards: the decoder's reference chain only runs forwards, so
            // the stream is re-entered at the keyframe at or before the
            // target and replayed to it.
            if let Some(decoder) = self.decoder.as_ref() {
                decoder.reset();
            }
            self.last_fed_index = -1;
            self.needs_parameter_sets = true;
            self.shown_stamp = None;
            keyframe_at_or_before(&self.samples, target_index)
        };

        for index in start..=target_index {
            // The browser's decode queue is the browser's, and the only back
            // pressure this side has is to stop handing it units. A backward
            // seek in a long group of pictures, or a file with no keyframe
            // the walk recognised, would otherwise submit the whole run at
            // once, a tab's memory spent on compressed frames whose pictures
            // are obsolete before they are drawn. Stopping is safe because
            // `last_fed_index` records where it stopped and the player asks
            // again on its next tick, which resumes forwards from here.
            if self
                .decoder
                .as_ref()
                .is_none_or(|decoder| decoder.queued() >= MAX_QUEUED_UNITS)
            {
                break;
            }
            let Some(sample) = self.samples.get(index) else {
                break;
            };
            // The parameter sets ride with the first unit after a reset.
            let unit = if self.needs_parameter_sets {
                self.needs_parameter_sets = false;
                let mut with_sets = self.sps_pps.clone();
                with_sets.extend_from_slice(&sample.data);
                with_sets
            } else {
                sample.data.clone()
            };
            if let Some(decoder) = self.decoder.as_ref() {
                decoder.decode(&unit, stamp_of(index), sample.is_keyframe);
            }
            self.last_fed_index = index as i32;
        }
        self.collect();
    }

    /// Take whatever the decoder has produced since the last look.
    ///
    /// The index comes back out of the stamp the sample was fed under, which
    /// is what makes a picture identify itself: pictures arrive in decode
    /// order and a seek feeds several, so "what was asked for" is the wrong
    /// label for the first of them.
    fn collect(&mut self) {
        let Some(picture) = self.decoder.as_ref().and_then(webcodecs::Decoder::newest) else {
            return;
        };
        let stamp = i32::try_from(picture.timestamp_micros).unwrap_or(i32::MAX);
        if self.shown_stamp == Some(stamp) {
            return;
        }
        self.shown_stamp = Some(stamp);
        // The stamp *is* the index, so nothing has to be inverted. The
        // position shown is computed from it rather than carried on the
        // picture, because a WebCodecs timestamp is a label and this one is
        // deliberately not a clock.
        let index = usize::try_from(stamp.max(0)).unwrap_or(0);
        self.current_frame = Some(StreamingFrame {
            image: picture.image,
            timestamp: self.frame_duration.mul_f64(index as f64),
            index: index.min(self.samples.len().saturating_sub(1)),
        });
    }

    /// Why the decoder stopped, if it has.
    ///
    /// Read by the player each time it asks for a frame: a decode that fails
    /// after `configure` — a chunk refused, the error callback firing, a copy
    /// that would not complete — otherwise leaves it playing with no picture
    /// and nothing to say.
    pub fn failure(&self) -> Option<String> {
        self.decoder.as_ref().and_then(webcodecs::Decoder::failure)
    }

    /// The newest decoded frame, if one has arrived yet.
    #[must_use]
    pub fn current_frame(&self) -> Option<&StreamingFrame> {
        self.current_frame.as_ref()
    }

    /// Back to the beginning, with frame zero on its way.
    ///
    /// The decode is started here rather than left to the next poll, because
    /// the player asks for `current_frame` immediately after resetting and a
    /// slot cleared with nothing requested leaves a finished video showing
    /// its last frame for ever.
    pub fn reset(&mut self) {
        if let Some(decoder) = self.decoder.as_ref() {
            decoder.reset();
        }
        self.last_fed_index = -1;
        self.needs_parameter_sets = true;
        self.shown_stamp = None;
        self.current_frame = None;
        self.seek_to_frame(0);
    }

    pub fn take_audio(&mut self) -> Option<VideoAudio> {
        self.audio.take()
    }
}
