//! A video attachment, decoded by the browser.
//!
//! The same job as the desktop half beside it and much less code,
//! because only the decoder differs: [`super::demux`] reads the container on
//! both targets, and what openh264 does there [`super::webcodecs`] does here.
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

use std::time::Duration;

use anyhow::{Result, anyhow};

use super::audio::VideoAudio;
use super::demux::{StreamingFrame, Track, stamp_of};
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

/// An attachment's samples, and the browser decoder reading them.
pub struct StreamingVideoDecoder {
    /// The container's video track: its samples, its parameter sets and its
    /// timing, read by the demux both targets share.
    ///
    /// The parameter sets are prepended to the first unit fed after every
    /// reset, because a browser decoder configured without a `description`
    /// learns the geometry from the stream and a reset forgets it.
    track: Track,
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
    ///
    /// A *decode* index, and the only one this type holds: everything a
    /// caller passes in or reads back is a presentation rank. The two agree
    /// on a baseline stream and part company the moment one carries
    /// B-frames, which is why the cursor is named for which of them it is.
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
    /// Why the codec session could not be opened again, if it could not.
    ///
    /// A rebuild failure is not the decoder's own failure, because there is
    /// no decoder to hold it: `failure()` asks the session, and after a
    /// release there is none to ask. Logged alone, the player went on
    /// thinking itself playable, kept the frame it had, and waited for a
    /// picture nothing was going to produce. Cleared by a rebuild that works.
    rebuild_failed: Option<String>,
    current_frame: Option<StreamingFrame>,
}

impl StreamingVideoDecoder {
    /// Demux the container and configure the browser's decoder for it.
    ///
    /// # Errors
    ///
    /// No video track, a container this build cannot read, or a browser that
    /// will not decode the stream.
    pub fn new(mp4_data: &[u8]) -> Result<Self> {
        Self::attach(Track::read(mp4_data)?)
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
    pub(super) fn attach(track: Track) -> Result<Self> {
        // Built here rather than lazily, so a stream this browser will not
        // take is refused while somebody is still looking at the press that
        // asked for it.
        let decoder =
            webcodecs::Decoder::new(&track.sps_pps, track.rotation).map_err(|e| anyhow!(e))?;
        Ok(Self {
            track,
            decoder: Some(decoder),
            last_fed_index: -1,
            needs_parameter_sets: true,
            shown_stamp: None,
            rebuild_failed: None,
            current_frame: None,
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
            match webcodecs::Decoder::new(&self.track.sps_pps, self.track.rotation) {
                Ok(built) => {
                    // A new session knows nothing, so the next unit fed has
                    // to carry the parameter sets and the walk starts again.
                    self.last_fed_index = -1;
                    self.needs_parameter_sets = true;
                    self.shown_stamp = None;
                    self.rebuild_failed = None;
                    self.decoder = Some(built);
                }
                Err(e) => {
                    log::warn!("could not open a decoder for this video again: {e}");
                    self.rebuild_failed =
                        Some(format!("this video could not be opened again: {e}"));
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
        // A release is not a failure, and the next play is entitled to try.
        self.rebuild_failed = None;
        self.last_fed_index = -1;
        self.needs_parameter_sets = true;
        self.shown_stamp = None;
    }

    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.track.frame_count()
    }

    #[must_use]
    pub fn duration(&self) -> Duration {
        self.track.duration
    }

    pub fn seek(&mut self, time: Duration) {
        self.seek_to_frame(self.track.index_at(time));
    }

    /// Feed the decoder the samples the picture at `target_rank` depends on,
    /// and collect whatever has landed.
    ///
    /// `target_rank` is a position in presentation order, which is what every
    /// caller counts in; the samples are walked in decode order, which is the
    /// only place that order appears. On a stream with B-frames the two
    /// differ, and the walk below is "everything this picture depends on"
    /// rather than a range of positions.
    ///
    /// Returns having *asked* rather than having answered: the picture comes
    /// back on the browser's callback, so [`Self::current_frame`] is what
    /// eventually sees it. See the module note on why that is enough.
    pub fn seek_to_frame(&mut self, target_rank: usize) {
        if target_rank >= self.track.samples.len() {
            return;
        }
        if self
            .current_frame
            .as_ref()
            .is_some_and(|frame| frame.index == target_rank)
        {
            self.collect();
            return;
        }

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

        // Where the feed has to reach for this picture to have been produced,
        // which is not the sample the picture *is*: a decoder that reorders
        // has to have been given everything shown before it too. Compared
        // against the cursor rather than the rank, because the cursor is a
        // decode index and a B-frame stream is exactly the case where the two
        // disagree — and it is a running maximum, so playing forward walks
        // forward instead of reading every third frame as a backward seek.
        let target_decode = self.track.decode_through(target_rank);

        // Already fed, and its picture is on the way: the browser decodes on
        // its own schedule, so "asked for and not arrived" is the ordinary
        // state a moment after a seek. Resetting here threw away the very
        // work that was about to answer, and then replayed the whole group of
        // pictures to ask for it again.
        if !refused
            && i64::try_from(target_decode).unwrap_or(i64::MAX) == i64::from(self.last_fed_index)
        {
            self.collect();
            return;
        }

        // A refusal takes the replay path whatever the target is, because
        // what has to be redone is a picture and not a position: the decoder
        // is re-entered at the keyframe before the target and walked to it,
        // which is exactly what a backward seek already does.
        let start = if !refused && target_decode as i32 > self.last_fed_index {
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
            self.track.keyframe_for(target_rank)
        };

        for index in start..=target_decode {
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
            let Some(sample) = self.track.samples.get(index) else {
                break;
            };
            // The parameter sets ride with the first unit after a reset.
            let unit = if self.needs_parameter_sets {
                self.needs_parameter_sets = false;
                let mut with_sets = self.track.sps_pps.clone();
                with_sets.extend_from_slice(&sample.data);
                with_sets
            } else {
                sample.data.clone()
            };
            if let Some(decoder) = self.decoder.as_ref() {
                // Stamped with where the picture is *shown*, not with where
                // it was fed: the browser answers in presentation order and
                // hands the label back, so a decode-order label comes back
                // out of sequence on any stream with B-frames.
                decoder.decode(
                    &unit,
                    stamp_of(self.track.rank_of(index)),
                    sample.is_keyframe,
                );
            }
            self.last_fed_index = index as i32;
        }
        self.collect();
    }

    /// Take whatever the decoder has produced since the last look.
    ///
    /// The rank comes back out of the stamp the sample was fed under, which
    /// is what makes a picture identify itself: a seek feeds several samples
    /// and the browser answers them in its own order, so "what was asked for"
    /// is the wrong label for the first of them.
    fn collect(&mut self) {
        let Some(picture) = self.decoder.as_ref().and_then(webcodecs::Decoder::newest) else {
            return;
        };
        let stamp = i32::try_from(picture.timestamp_micros).unwrap_or(i32::MAX);
        if self.shown_stamp == Some(stamp) {
            return;
        }
        self.shown_stamp = Some(stamp);
        // The stamp *is* the rank, so nothing has to be inverted. The
        // position shown is looked up from it rather than carried on the
        // picture, because a WebCodecs timestamp is a label and this one is
        // deliberately not a clock.
        let rank = usize::try_from(stamp.max(0))
            .unwrap_or(0)
            .min(self.track.samples.len().saturating_sub(1));
        self.current_frame = Some(StreamingFrame {
            image: picture.image,
            timestamp: self.track.timestamp_of(rank),
            index: rank,
        });
    }

    /// Why the decoder stopped, if it has.
    ///
    /// Read by the player each time it asks for a frame: a decode that fails
    /// after `configure` — a chunk refused, the error callback firing, a copy
    /// that would not complete — otherwise leaves it playing with no picture
    /// and nothing to say.
    pub fn failure(&self) -> Option<String> {
        // The rebuild first, because it is the case with no decoder to ask.
        self.rebuild_failed
            .clone()
            .or_else(|| self.decoder.as_ref().and_then(webcodecs::Decoder::failure))
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
        self.track.audio.take()
    }
}
