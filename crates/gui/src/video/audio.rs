//! Audio extraction from video files
//!
//! This module provides audio extraction functionality for MP4 video files.
//! The video decoding is handled by StreamingVideoDecoder in streaming.rs.
//!
//! [`VideoAudio`] is the whole module on the web: the player and the
//! call's stub both name it, so the *type* is every target's. What is not is
//! the extraction — `mp4` demuxes and `symphonia` decodes the AAC, and the
//! only thing that calls it is `streaming.rs`, which a page does not build.
//! Gated rather than left to the optimizer, which is the discipline
//! `openh264` already follows two lines below it in the manifest: LTO does in
//! fact remove all of it today (measured — `symphonia` and `mp4` are absent
//! from the shipped module), but `get_probe()` builds its registry through
//! trait objects, which is exactly the shape dead-code elimination is least
//! reliable about. A page that stopped being able to prove it would gain a
//! codec it cannot reach.

/// ADTS sample rate to frequency index mapping
#[cfg(not(target_family = "wasm"))]
const ADTS_FREQ_TABLE: [(u32, u8); 13] = [
    (96000, 0),
    (88200, 1),
    (64000, 2),
    (48000, 3),
    (44100, 4),
    (32000, 5),
    (24000, 6),
    (22050, 7),
    (16000, 8),
    (12000, 9),
    (11025, 10),
    (8000, 11),
    (7350, 12),
];

/// Decoded audio data from video (always mono after conversion)
///
/// Shared rather than owned, because the play path hands this around: the
/// player keeps one, a resume re-feeds one, and each of those used to be a
/// copy of the whole track. Three minutes of mono at 44.1 kHz is 31 MB in
/// `f32`, and on the web that is linear memory that never comes back.
#[derive(Clone)]
pub struct VideoAudio {
    /// PCM samples (f32, mono)
    pub samples: std::sync::Arc<[f32]>,
    /// Sample rate in Hz
    pub sample_rate: u32,
}

/// Extract audio from MP4 using mp4 crate for demuxing and symphonia for AAC decoding
#[cfg(not(target_family = "wasm"))]
pub fn extract_audio_from_mp4(mp4_data: &[u8]) -> Option<VideoAudio> {
    use std::io::Cursor;

    use mp4::{Mp4Reader, TrackType};

    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    // First, use mp4 crate to find and extract audio track
    let cursor = Cursor::new(mp4_data);
    let mut mp4 = match Mp4Reader::read_header(cursor, mp4_data.len() as u64) {
        Ok(mp4) => mp4,
        Err(e) => {
            log::warn!("Failed to read MP4 for audio extraction: {}", e);
            return None;
        }
    };

    // Find audio track
    let audio_track = mp4
        .tracks()
        .values()
        .find(|t| matches!(t.track_type(), Ok(TrackType::Audio)))?;

    let track_id = audio_track.track_id();
    let sample_count = audio_track.sample_count();

    // Get audio parameters
    let sample_rate = audio_track
        .sample_freq_index()
        .ok()
        .map(|f| f.freq())
        .unwrap_or(44100);
    let channels = audio_track
        .channel_config()
        .ok()
        .map(|c| c as u8)
        .unwrap_or(2);

    log::info!(
        "Audio track found: id={}, {} samples, {} Hz, {} channels",
        track_id,
        sample_count,
        sample_rate,
        channels
    );

    // Straight into the ADTS stream, rather than collecting every frame and
    // copying the lot again: the two together held the whole track twice
    // over, on top of the MP4 the caller is still holding, before a sample
    // had been decoded.
    let mut adts_data = Vec::with_capacity(mp4_data.len());
    let mut frames = 0usize;
    for sample_idx in 1..=sample_count {
        if let Ok(Some(sample)) = mp4.read_sample(track_id, sample_idx) {
            push_adts_frame(&mut adts_data, &sample.bytes, sample_rate, channels);
            frames += 1;
        }
    }
    drop(mp4);

    if frames == 0 {
        log::info!("No AAC frames extracted");
        return None;
    }

    log::info!(
        "Extracted {} AAC frames from MP4 as {} bytes of ADTS",
        frames,
        adts_data.len()
    );

    // Now decode ADTS using symphonia
    let cursor = Cursor::new(adts_data);
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let mut hint = Hint::new();
    hint.with_extension("aac");
    let format_opts = FormatOptions::default();
    let metadata_opts = MetadataOptions::default();

    let mut format =
        match symphonia::default::get_probe().probe(&hint, mss, format_opts, metadata_opts) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("Failed to probe ADTS format: {}", e);
                return None;
            }
        };

    // Find the audio track in ADTS
    let track = format.tracks().first()?;

    let adts_track_id = track.id;
    let Some(CodecParameters::Audio(audio_params)) = track.codec_params.clone() else {
        log::warn!("ADTS track carries no audio codec parameters");
        return None;
    };
    let decoder_opts = AudioDecoderOptions::default();
    let mut decoder =
        match symphonia::default::get_codecs().make_audio_decoder(&audio_params, &decoder_opts) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("Failed to create AAC decoder: {}", e);
                return None;
            }
        };

    let mut all_samples: Vec<f32> = Vec::new();
    let mut frame: Vec<f32> = Vec::new();

    // Decode all audio packets
    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id != adts_track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                decoded.copy_to_vec_interleaved(&mut frame);
                all_samples.extend_from_slice(&frame);
            }
            Err(e) => {
                log::debug!("Audio decode error (skipping frame): {}", e);
            }
        }
    }

    if all_samples.is_empty() {
        log::info!("No audio samples decoded");
        return None;
    }

    // Downmix to mono if needed (average across all channels; the API promises mono)
    let mono_samples = if channels > 1 {
        let mono: Vec<f32> = all_samples
            .chunks(channels as usize)
            .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32)
            .collect();
        log::info!(
            "Converted {} interleaved samples to {} mono samples",
            all_samples.len(),
            mono.len()
        );
        mono
    } else {
        all_samples
    };

    log::info!(
        "Decoded {} audio samples ({:.2}s)",
        mono_samples.len(),
        mono_samples.len() as f32 / sample_rate as f32
    );

    Some(VideoAudio {
        samples: mono_samples.into(),
        sample_rate,
    })
}

/// The largest an ADTS frame may say it is: the length field is 13 bits.
const MAX_ADTS_FRAME: usize = (1 << 13) - 1;

/// One AAC frame, headered, appended to the ADTS stream being built.
fn push_adts_frame(adts: &mut Vec<u8>, frame: &[u8], sample_rate: u32, channels: u8) {
    // Map sample rate to ADTS frequency index using lookup table
    let freq_idx = ADTS_FREQ_TABLE
        .iter()
        .find(|(rate, _)| *rate == sample_rate)
        .map(|(_, idx)| *idx)
        .unwrap_or(4); // Default to 44100 (index 4)

    // ADTS profile field stores Audio Object Type minus one; AAC-LC AOT = 2
    let profile = 2u8; // AAC-LC

    // The channel configuration is three bits and 0 means "read it from an
    // inline config", which there is none of here. A track answering
    // something outside 1..=7 is clamped rather than spread across the two
    // fields it is masked into, where it would silently name another layout.
    let channels = channels.clamp(1, 7);

    let frame_len = frame.len() + 7; // ADTS header is 7 bytes
    // The length field is 13 bits, and the masks below simply drop what does
    // not fit: the header would then claim a shorter frame, the parser would
    // resynchronise somewhere inside it, and the rest of the track would be
    // discarded with nothing said. Skipped and named instead.
    if frame_len > MAX_ADTS_FRAME {
        log::warn!("skipping a {frame_len} byte AAC frame, past what ADTS can describe");
        return;
    }

    // Build 7-byte ADTS header
    let header: [u8; 7] = [
        0xFF,
        0xF1, // Syncword + MPEG-4 + no CRC
        ((profile - 1) << 6) | (freq_idx << 2) | ((channels >> 2) & 0x01),
        ((channels & 0x03) << 6) | ((frame_len >> 11) & 0x03) as u8,
        ((frame_len >> 3) & 0xFF) as u8,
        (((frame_len & 0x07) << 5) | 0x1F) as u8,
        0xFC, // Buffer fullness VBR + 0 frames - 1
    ];
    adts.extend_from_slice(&header);
    adts.extend_from_slice(frame);
}

/// The same, over a whole track already in hand.
///
/// The extraction above streams straight into its buffer rather than
/// collecting the frames first; this exists for the tests, which are about
/// what one frame's header says.
#[cfg(test)]
fn wrap_aac_as_adts(frames: &[Vec<u8>], sample_rate: u32, channels: u8) -> Vec<u8> {
    let mut adts = Vec::new();
    for frame in frames {
        push_adts_frame(&mut adts, frame, sample_rate, channels);
    }
    adts
}

#[cfg(test)]
mod tests {
    use super::{MAX_ADTS_FRAME, wrap_aac_as_adts};

    /// The `aac_frame_length` field is 13 bits and the masks that build it
    /// drop what does not fit, so an oversized frame produced a header
    /// claiming a shorter one: symphonia resynchronises somewhere inside the
    /// frame and the rest of the track is discarded, leaving a video with
    /// part of its audio and nothing in the log about it.
    #[test]
    fn a_frame_too_large_for_adts_is_skipped_rather_than_mislabelled() {
        let big = vec![0u8; MAX_ADTS_FRAME];
        let small = vec![0u8; 16];
        let wrapped = wrap_aac_as_adts(&[big, small.clone()], 44100, 2);

        assert_eq!(
            wrapped.len(),
            small.len() + 7,
            "only the frame that fits is described"
        );
        let claimed = usize::from(wrapped[3] & 0x03) << 11
            | usize::from(wrapped[4]) << 3
            | usize::from(wrapped[5] >> 5);
        assert_eq!(claimed, small.len() + 7, "and it is described honestly");
    }

    /// The channel configuration is three bits, spread across two fields. A
    /// track answering something outside 1..=7 wrote into the bits beside
    /// them, naming a different layout and a different frame length.
    #[test]
    fn an_impossible_channel_count_does_not_reach_the_header() {
        for channels in [0u8, 8, 255] {
            let wrapped = wrap_aac_as_adts(&[vec![0u8; 8]], 44100, channels);
            let config = (wrapped[2] & 0x01) << 2 | wrapped[3] >> 6;
            assert!(
                (1..=7).contains(&config),
                "{channels} channels became {config}"
            );
            let claimed = usize::from(wrapped[3] & 0x03) << 11
                | usize::from(wrapped[4]) << 3
                | usize::from(wrapped[5] >> 5);
            assert_eq!(claimed, 8 + 7, "and the length beside it is untouched");
        }
    }
}
