//! Changing a clip's sample rate, once.
//!
//! Three places needed it and each had grown its own answer: a call's device
//! bridge built a windowed-sinc filter and documented why one is necessary,
//! the recorder built the same filter for whole-number decimation and fell
//! back to bare linear interpolation for everything else, and playback simply
//! took the nearest sample. A 44.1kHz device is not a whole-number step down
//! to 16kHz, so a voice note recorded on one went out through the branch with
//! no filter at all, folding everything above 8kHz back into the voice band.
//!
//! The cursor is `f64` for a reason of its own: `f32` carries 24 bits of
//! mantissa, so past about 16.7 million the integers stop being exact — which
//! is six minutes of 48kHz audio, after which a clip resamples by duplicating
//! and skipping samples at a rate that grows with its length.

/// How many taps the low-pass gets. Enough for a transition band that does not
/// eat the passband at the ratios a phone and a sound card produce.
const LOWPASS_TAPS: usize = 63;

/// Windowed-sinc low-pass, Hamming window, normalized to unity DC gain.
///
/// `cutoff` is in cycles per sample of the *input* rate.
pub(crate) fn lowpass_taps(cutoff: f32) -> Vec<f32> {
    taps_of(cutoff, LOWPASS_TAPS)
}

pub(crate) fn taps_of(cutoff: f32, count: usize) -> Vec<f32> {
    let m = (count - 1) as f32;
    let mut taps: Vec<f32> = (0..count)
        .map(|n| {
            let x = n as f32 - m / 2.0;
            let sinc = if x.abs() < f32::EPSILON {
                2.0 * cutoff
            } else {
                (2.0 * std::f32::consts::PI * cutoff * x).sin() / (std::f32::consts::PI * x)
            };
            let window = 0.54 - 0.46 * (2.0 * std::f32::consts::PI * n as f32 / m).cos();
            sinc * window
        })
        .collect();
    let sum: f32 = taps.iter().sum();
    if sum.abs() > f32::EPSILON {
        for tap in &mut taps {
            *tap /= sum;
        }
    }
    taps
}

/// One clip, resampled: band-limited first when the rate is coming down, then
/// read with a fractional cursor and linear interpolation.
///
/// For whole clips held in memory — a voice note, a video's audio track. A
/// live stream carries filter history across its blocks instead; see
/// `call_device::Resampler`.
pub fn resample(samples: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if samples.is_empty() || src_rate == 0 || dst_rate == 0 || src_rate == dst_rate {
        return samples.to_vec();
    }

    // 0.45 rather than 0.5 of the destination Nyquist: leaves a transition
    // band, so the passband edge is not already rolling off.
    let band_limited = if src_rate > dst_rate {
        let taps = lowpass_taps(0.45 * dst_rate as f32 / src_rate as f32);
        Some(filtered(samples, &taps))
    } else {
        None
    };
    let source = band_limited.as_deref().unwrap_or(samples);

    let step = src_rate as f64 / dst_rate as f64;
    let output_len = ((source.len() as f64) / step).floor() as usize;
    let mut output = Vec::with_capacity(output_len);
    for out in 0..output_len {
        let at = out as f64 * step;
        let index = at as usize;
        let frac = (at - index as f64) as f32;
        let Some(&a) = source.get(index) else { break };
        let b = source.get(index + 1).copied().unwrap_or(a);
        output.push(a + (b - a) * frac);
    }
    output
}

/// `samples` through `taps`, with the edges clamped rather than zero-padded:
/// replicating the boundary sample beats fading the clip's ends.
pub(crate) fn filtered(samples: &[f32], taps: &[f32]) -> Vec<f32> {
    let center = (taps.len() - 1) / 2;
    let last = samples.len().saturating_sub(1) as isize;
    (0..samples.len())
        .map(|i| {
            taps.iter()
                .enumerate()
                .map(|(k, &tap)| {
                    let at = (i as isize + k as isize - center as isize).clamp(0, last);
                    tap * samples[at as usize]
                })
                .sum()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f32, secs: f32) -> Vec<f32> {
        let count = (rate as f32 * secs) as usize;
        (0..count)
            .map(|n| (std::f32::consts::TAU * hz * n as f32 / rate as f32).sin())
            .collect()
    }

    /// The rate a sound card actually runs at is often not a whole-number step
    /// down to 16kHz, and that branch had no filter in it: everything above
    /// 8kHz folded back into the voice band, which is the hiss on the note the
    /// peer receives.
    #[test]
    fn a_tone_above_the_target_nyquist_does_not_fold_back_into_the_band() {
        // 12 kHz at 44.1k has nowhere to go at 16k: it must be attenuated
        // rather than reappearing at 4 kHz.
        let out = resample(&tone(44_100, 12_000.0, 0.2), 44_100, 16_000);
        let energy: f32 = out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32;
        assert!(
            energy < 0.01,
            "an out-of-band tone must not survive the rate change: {energy}"
        );

        // And what is in band comes through.
        let kept = resample(&tone(44_100, 1_000.0, 0.2), 44_100, 16_000);
        let energy: f32 = kept.iter().map(|s| s * s).sum::<f32>() / kept.len() as f32;
        assert!(energy > 0.2, "speech has to survive it: {energy}");
    }

    /// `f32` stops counting integers exactly past ~16.7 million, which is six
    /// minutes of 48kHz audio: the cursor drifted, and the clip resampled by
    /// duplicating and skipping samples at a rate that grew with its length.
    ///
    /// Asserted of the arithmetic rather than of a clip, because filtering
    /// eight minutes of audio to prove it is a minute of test.
    #[test]
    fn the_cursor_stays_exact_over_a_long_clip() {
        let samples = 48_000usize * 60 * 8; // eight minutes
        let step = 3.0f64;
        let last_out = samples / 3 - 1;
        assert_eq!((last_out as f64 * step) as usize, samples - 3);
        assert_ne!(
            (last_out as f32 * step as f32) as usize,
            samples - 3,
            "which is the reading the cursor used to take"
        );
    }

    /// The length follows the cursor, and a short clip is not rounded up into
    /// samples that do not exist.
    #[test]
    fn a_clip_comes_back_at_the_length_the_rate_change_calls_for() {
        let src = tone(48_000, 440.0, 0.5);
        let out = resample(&src, 48_000, 16_000);
        assert_eq!(out.len(), src.len() / 3);
    }

    #[test]
    fn the_same_rate_is_the_same_samples() {
        let src = tone(16_000, 440.0, 0.05);
        assert_eq!(resample(&src, 16_000, 16_000), src);
    }
}

/// A resampler for a stream that arrives in blocks.
///
/// Shared rather than per-backend: a call is 16 kHz mono and no sound card is
/// -- cpal answers 44.1 or 48, and a browser's `AudioContext` answers whatever
/// the machine runs at -- so both ends of a call need this on both platforms.
/// The whole-clip [`resample`] above cannot stand in: it restarts its cursor
/// and its filter history at every call, which at a 20 ms block boundary is an
/// audible click sixty times a second.
///
/// Linear resampler that carries a fractional read cursor across calls so block
/// boundaries don't click.
///
/// When downsampling it first runs a windowed-sinc low-pass at the source rate.
/// Linear interpolation alone does not attenuate anything above the destination
/// Nyquist, so a 48 -> 16 kHz pull is effectively "take every third sample" and
/// folds everything above 8 kHz back into the voice band as aliasing.
/// Upsampling needs no such filter: interpolation cannot create content above
/// the source Nyquist.
pub(crate) struct Stream {
    src_rate: u32,
    dst_rate: u32,
    /// Fractional index into a virtual stream, carried across blocks.
    pos: f64,
    /// Empty when not downsampling.
    taps: Vec<f32>,
    /// `taps.len() - 1` samples of the previous block, so the filter has
    /// history at a block boundary instead of ringing from zeros.
    history: Vec<f32>,
    /// Scratch for the filtered block; reused to keep the drain allocation-free.
    filtered: Vec<f32>,
}

impl Stream {
    pub(crate) fn new(src_rate: u32, dst_rate: u32) -> Self {
        let taps = if src_rate > dst_rate {
            // 0.45 rather than 0.5 of the destination Nyquist: leaves a
            // transition band so the passband edge is not already rolling off.
            lowpass_taps(0.45 * dst_rate as f32 / src_rate as f32)
        } else {
            Vec::new()
        };
        let history = vec![0.0; taps.len().saturating_sub(1)];
        Self {
            src_rate,
            dst_rate,
            pos: 0.0,
            taps,
            history,
            filtered: Vec::new(),
        }
    }

    /// Resample `src` into `out`. Allocation-free past the warmup.
    pub(crate) fn process(&mut self, src: &[i16], out: &mut Vec<i16>) {
        if src.is_empty() {
            return;
        }
        let step = self.src_rate as f64 / self.dst_rate as f64;

        // Band-limit at the source rate before the cursor decimates.
        let filtered: &[f32] = if self.taps.is_empty() {
            self.filtered.clear();
            self.filtered.extend(src.iter().map(|&s| s as f32));
            &self.filtered
        } else {
            self.filtered.clear();
            self.filtered.reserve(src.len());
            let hist = self.history.len();
            for i in 0..src.len() {
                let mut acc = 0.0f32;
                for (k, &tap) in self.taps.iter().enumerate() {
                    // Tap k reads k samples back; anything before this block
                    // comes out of the carried history.
                    let idx = i as isize - k as isize;
                    let sample = if idx >= 0 {
                        src[idx as usize] as f32
                    } else {
                        let h = hist as isize + idx;
                        if h >= 0 {
                            self.history[h as usize]
                        } else {
                            0.0
                        }
                    };
                    acc += tap * sample;
                }
                self.filtered.push(acc);
            }
            // Carry this block's tail as the next block's history.
            if hist > 0 {
                let keep = hist.min(src.len());
                self.history.rotate_left(keep);
                let start = self.history.len() - keep;
                for (slot, &s) in self.history[start..]
                    .iter_mut()
                    .zip(&src[src.len() - keep..])
                {
                    *slot = s as f32;
                }
            }
            &self.filtered
        };

        let mut p = self.pos;
        while p < filtered.len() as f64 {
            let i = p as usize;
            let frac = (p - i as f64) as f32;
            let a = filtered[i];
            let b = if i + 1 < filtered.len() {
                filtered[i + 1]
            } else {
                a
            };
            out.push((a + (b - a) * frac).round().clamp(-32768.0, 32767.0) as i16);
            p += step;
        }
        // Carry the leftover fraction (relative to the next block's start) so
        // the next call continues smoothly instead of restarting at 0.
        self.pos = p - filtered.len() as f64;
    }
}
