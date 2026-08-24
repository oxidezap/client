//! Waveform generation for WhatsApp PTT voice messages.

pub const WAVEFORM_SAMPLES: usize = 64;
const MAX_AMPLITUDE: u8 = 100;

/// Generate a 64-byte waveform from audio samples using RMS.
pub fn generate_waveform(samples: &[f32]) -> Vec<u8> {
    if samples.is_empty() {
        return vec![0u8; WAVEFORM_SAMPLES];
    }

    // Every bucket gets a slice of the clip, so a message shorter than one
    // sample per bucket still renders across the full width. Chunking by a
    // ceiling-divided size instead would emit fewer than WAVEFORM_SAMPLES
    // values and pad the rest with zeros, drawing the tail as silence.
    let len = samples.len();
    let rms_values: Vec<f32> = (0..WAVEFORM_SAMPLES)
        .map(|i| {
            let start = i * len / WAVEFORM_SAMPLES;
            // At least one sample per bucket: with len < WAVEFORM_SAMPLES the
            // proportional end would collapse onto the start.
            let end = (((i + 1) * len).div_ceil(WAVEFORM_SAMPLES)).clamp(start + 1, len);
            let chunk = &samples[start..end];
            let sum_squares: f32 = chunk.iter().map(|s| s * s).sum();
            (sum_squares / chunk.len() as f32).sqrt()
        })
        .collect();

    let max_rms = rms_values.iter().copied().fold(f32::MIN, f32::max);
    if max_rms < f32::EPSILON {
        return vec![0u8; WAVEFORM_SAMPLES];
    }

    rms_values
        .iter()
        .map(|rms| ((rms / max_rms) * MAX_AMPLITUDE as f32) as u8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_waveform_length() {
        let samples = vec![0.5f32; 1000];
        let waveform = generate_waveform(&samples);
        assert_eq!(waveform.len(), WAVEFORM_SAMPLES);
    }

    #[test]
    fn test_waveform_range() {
        let samples: Vec<f32> = (0..10000).map(|i| (i as f32 / 100.0).sin()).collect();
        let waveform = generate_waveform(&samples);
        for &val in &waveform {
            assert!(val <= MAX_AMPLITUDE);
        }
    }

    #[test]
    fn test_empty_samples() {
        let waveform = generate_waveform(&[]);
        assert_eq!(waveform.len(), WAVEFORM_SAMPLES);
        assert!(waveform.iter().all(|&v| v == 0));
    }

    #[test]
    fn test_short_clip_fills_every_bucket() {
        // 65 samples used to produce 33 RMS values padded with 31 zeros,
        // rendering half the clip as silence.
        let samples = vec![0.5f32; 65];
        let waveform = generate_waveform(&samples);
        assert_eq!(waveform.len(), WAVEFORM_SAMPLES);
        assert!(waveform.iter().all(|&v| v == MAX_AMPLITUDE));
    }

    #[test]
    fn test_fewer_samples_than_buckets() {
        let samples = vec![0.5f32; 3];
        let waveform = generate_waveform(&samples);
        assert_eq!(waveform.len(), WAVEFORM_SAMPLES);
        assert!(waveform.iter().all(|&v| v == MAX_AMPLITUDE));
    }

    #[test]
    fn test_silent_audio() {
        let samples = vec![0.0f32; 10000];
        let waveform = generate_waveform(&samples);
        assert!(waveform.iter().all(|&v| v == 0));
    }
}
