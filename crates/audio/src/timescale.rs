//! Playing a voice note faster without turning the speaker into a chipmunk.
//!
//! The obvious way to play at 2× is to read the samples twice as fast, which
//! also doubles every frequency in them. For music that is a novelty; for
//! speech it is unusable, and it is not what "1.5×" means on any player a
//! person has used.
//!
//! So the waveform is re-timed instead of re-pitched: WSOLA — overlap-add,
//! with each frame nudged to where it best lines up with what was already
//! written. The nudge is the whole trick. Plain overlap-add drops frames onto
//! a fixed grid and the periods of a voice fight each other into a hollow,
//! phasey sound; searching a few milliseconds either way for the offset with
//! the strongest correlation keeps the pitch periods in step.
//!
//! One pass over the clip, at decode time, into one allocation the size of the
//! result. Nothing here runs on the audio thread.

/// Frame length, in frames-per-channel. ~40ms at 48kHz: long enough to hold a
/// pitch period of the lowest speaking voice, short enough that a syllable is
/// not smeared across it.
const FRAME: usize = 1920;

/// How far the alignment search may move a frame, in frames-per-channel.
/// ~5ms, which covers a period at 200Hz and up.
const SEARCH: usize = 240;

/// Speeds close enough to 1 that re-timing would only add artefacts.
const UNCHANGED: f32 = 0.01;

/// Re-time `samples` by `speed` without changing its pitch.
///
/// `samples` is interleaved across `channels`. A `speed` of 1 (or a degenerate
/// input) returns the samples untouched, so the common path pays nothing.
pub fn stretch(samples: Vec<f32>, channels: usize, speed: f32) -> Vec<f32> {
    let channels = channels.max(1);
    if !speed.is_finite()
        || speed <= 0.0
        || (speed - 1.0).abs() < UNCHANGED
        || samples.len() < (FRAME + SEARCH) * channels * 2
    {
        return samples;
    }

    let frame = FRAME * channels;
    let search = SEARCH * channels;
    // How far to advance through the *input* per output frame. Faster than 1×
    // means stepping further in than out, which is where time is lost.
    let hop_out = frame / 2;
    let hop_in = ((hop_out as f32) * speed).round() as usize;
    let hop_in = hop_in.max(channels);

    let mut out: Vec<f32> = Vec::with_capacity((samples.len() as f32 / speed) as usize + frame);
    out.extend_from_slice(&samples[..frame.min(samples.len())]);

    let window = hann(frame / channels);
    // The nominal read position advances by exactly `hop_in` every time. The
    // alignment search moves the frame that is *taken*, never the cursor:
    // feeding the chosen offset back in accumulates its error, and a clip a
    // minute long drifts far enough off the requested speed to be wrong.
    let mut read = hop_in;

    while read + frame + search < samples.len() {
        // Where the tail of what has been written already sits, so the next
        // frame can be matched against it.
        let overlap_at = out.len() - hop_out;
        let best = best_offset(&samples, read, &out[overlap_at..], frame, search, channels);
        let take = &samples[best..best + frame];

        // Cross-fade the first half of the new frame over the last half of
        // what is there, then append the rest of it.
        for i in 0..hop_out {
            let w = window[(i / channels).min(window.len() - 1)];
            let at = overlap_at + i;
            out[at] = out[at] * (1.0 - w) + take[i] * w;
        }
        out.extend_from_slice(&take[hop_out..]);

        read += hop_in;
    }

    out
}

/// The offset within the search window whose frame best continues `tail`.
///
/// Plain cross-correlation over the first half-frame. The candidates are
/// stepped by whole frames-per-channel so a stereo pair is never split, which
/// would swap the channels for the rest of the clip.
fn best_offset(
    samples: &[f32],
    from: usize,
    tail: &[f32],
    frame: usize,
    search: usize,
    channels: usize,
) -> usize {
    let compare = tail.len().min(frame / 2);
    if compare == 0 {
        return from;
    }

    let start = from.saturating_sub(search / 2);
    let end = (from + search / 2).min(samples.len().saturating_sub(frame));
    let mut best = from.min(end);
    let mut best_score = f32::NEG_INFINITY;

    let mut at = start - (start % channels);
    while at <= end {
        let candidate = &samples[at..at + compare];
        let score: f32 = tail[..compare]
            .iter()
            .zip(candidate)
            .map(|(a, b)| a * b)
            .sum();
        if score > best_score {
            best_score = score;
            best = at;
        }
        at += channels;
    }
    best
}

/// A raised cosine, so the two halves of a cross-fade sum to one.
fn hann(len: usize) -> Vec<f32> {
    (0..len.max(1))
        .map(|i| {
            let phase = std::f32::consts::PI * i as f32 / len.max(1) as f32;
            phase.sin().powi(2)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second of a 220Hz tone, the sort of pitch a speaking voice sits near.
    fn tone(secs: f32, channels: usize) -> Vec<f32> {
        let rate = 48_000.0;
        let frames = (secs * rate) as usize;
        (0..frames)
            .flat_map(|i| {
                let value = (2.0 * std::f32::consts::PI * 220.0 * i as f32 / rate).sin() * 0.5;
                std::iter::repeat_n(value, channels)
            })
            .collect()
    }

    /// The dominant frequency, by counting zero crossings — enough to tell a
    /// re-timed clip from a re-pitched one, which is the whole question.
    fn crossings(samples: &[f32], channels: usize) -> usize {
        samples
            .chunks(channels)
            .map(|frame| frame[0])
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count()
    }

    #[test]
    fn one_times_speed_is_the_same_samples() {
        let samples = tone(0.5, 1);
        assert_eq!(stretch(samples.clone(), 1, 1.0), samples);
        assert_eq!(stretch(samples.clone(), 1, 1.004), samples);
    }

    #[test]
    fn a_nonsense_speed_changes_nothing() {
        let samples = tone(0.2, 1);
        assert_eq!(stretch(samples.clone(), 1, 0.0), samples);
        assert_eq!(stretch(samples.clone(), 1, -2.0), samples);
        assert_eq!(stretch(samples.clone(), 1, f32::NAN), samples);
    }

    #[test]
    fn two_times_speed_halves_the_length() {
        let samples = tone(1.0, 1);
        let fast = stretch(samples.clone(), 1, 2.0);
        let ratio = fast.len() as f32 / samples.len() as f32;
        assert!(
            (ratio - 0.5).abs() < 0.05,
            "expected about half as long, got {ratio}"
        );
    }

    /// The point of all of it: half as long, same pitch. A player that simply
    /// read faster would come out with the same *number* of crossings as the
    /// original, over half the time — an octave up.
    #[test]
    fn speeding_up_does_not_raise_the_pitch() {
        let samples = tone(1.0, 1);
        let before = crossings(&samples, 1);
        let after = crossings(&stretch(samples, 1, 2.0), 1);
        let ratio = after as f32 / before as f32;
        assert!(
            (ratio - 0.5).abs() < 0.1,
            "half the crossings means the same pitch over half the time; got {ratio}"
        );
    }

    #[test]
    fn a_stereo_clip_keeps_its_channels_paired() {
        // Both channels carry the same tone, so any frame split would show up
        // as the two disagreeing.
        let samples = tone(1.0, 2);
        let fast = stretch(samples, 2, 1.5);
        assert_eq!(
            fast.len() % 2,
            0,
            "a stereo clip stays a whole number of frames"
        );
        let mismatched = fast
            .chunks(2)
            .filter(|frame| (frame[0] - frame[1]).abs() > 1e-6)
            .count();
        assert_eq!(mismatched, 0, "the channels stayed paired");
    }

    #[test]
    fn slowing_down_lengthens_the_clip() {
        let samples = tone(1.0, 1);
        let slow = stretch(samples.clone(), 1, 0.5);
        let ratio = slow.len() as f32 / samples.len() as f32;
        assert!(
            (ratio - 2.0).abs() < 0.1,
            "expected twice as long, got {ratio}"
        );
    }
}
