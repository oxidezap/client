//! Audio extraction from video files
//!
//! This module provides audio extraction functionality for MP4 video files.
//! The video decoding is handled by StreamingVideoDecoder in `native.rs` and
//! `web.rs`.
//!
//! The extraction was desktop-only while the only caller was `native.rs`,
//! which a page did not build. It builds one now — `web.rs` demuxes
//! the same container for the browser's own decoder — so a video's sound
//! plays on both, and `symphonia` and `mp4` are in the shipped module rather
//! than removed from it by LTO. That is a real cost, paid for a real feature:
//! a video that played silently on the web would be the more obvious defect.

/// ADTS sample rate to frequency index mapping
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

    // The count is the container's, and the container is a file somebody
    // sent. Two ceilings, because neither alone is one: the file's length
    // bounds what it can carry, since a sample is at least a byte and `stsz`
    // can otherwise declare four billion in a header that costs nothing to
    // write; but a fixed one-byte sample size makes that bound tens of
    // millions on its own, and the cost there is one `Vec` of metadata each
    // before a byte of payload is read. Without both, the loop below is the
    // denial of service rather than the allocation.
    let ceiling = mp4_data.len().min(super::demux::MAX_TRACK_SAMPLES);
    let sample_count = sample_count.min(u32::try_from(ceiling).unwrap_or(u32::MAX));

    // Read straight into the ADTS stream, rather than collecting every frame
    // and copying the lot again: the two together held the whole track twice
    // over, on top of the MP4 the caller is still holding, before a sample
    // had been decoded. The ceiling above still bounds it — what that bounds
    // is how many samples are read, which is a question about the container
    // rather than about where they are put.
    //
    // Grown rather than reserved: the audio is a small fraction of a video
    // file, and reserving the MP4's whole length asked for a second copy of
    // it beside the one the caller is still holding.
    let mut adts_data = Vec::new();
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

    // Mono as it comes, rather than a whole interleaved track and then a
    // downmix of it. The result of this function is mono, so keeping the
    // interleaved copy meant holding both at once: a stereo track's peak was
    // one and a half times what it ends up returning, for a buffer that
    // exists only to be averaged. Averaging per decoded frame is the same
    // arithmetic against a buffer the size of one frame.
    let mut mono_samples: Vec<f32> = Vec::new();
    let mut frame: Vec<f32> = Vec::new();

    // Decode all audio packets
    while let Ok(Some(packet)) = format.next_packet() {
        if packet.track_id != adts_track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                decoded.copy_to_vec_interleaved(&mut frame);
                // The bound above is on the *packets*, and this is where the
                // cost actually is: an AAC packet is a few hundred bytes and
                // decodes to 1024 samples a channel, so a low-bitrate track
                // inside every ceiling so far still expands into billions of
                // `f32`. Truncated rather than refused, which is what this
                // function already does with a frame it cannot describe: a
                // video with its first hours of sound is better than a tab
                // that aborted opening it.
                if mono_samples.len() + frame.len() > MAX_DECODED_SAMPLES {
                    log::warn!(
                        "truncating a video's audio at {} samples: the track decodes to more \
                         than this build will hold",
                        mono_samples.len()
                    );
                    break;
                }
                if channels > 1 {
                    mono_samples.extend(
                        frame
                            .chunks(channels as usize)
                            .map(|chunk| chunk.iter().sum::<f32>() / chunk.len() as f32),
                    );
                } else {
                    mono_samples.extend_from_slice(&frame);
                }
            }
            Err(e) => {
                log::debug!("Audio decode error (skipping frame): {}", e);
            }
        }
    }

    if mono_samples.is_empty() {
        log::info!("No audio samples decoded");
        return None;
    }

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

/// The most decoded audio a video attachment may expand into.
///
/// The packet ceiling is not this one, and the difference is the whole
/// reason both exist: a packet is a few hundred bytes of metadata and
/// decodes to 1024 samples per channel, so a track that satisfies every
/// bound on the *compressed* side still expands by three orders of
/// magnitude. On the web that is a linear memory with a fixed roof, where
/// running out aborts rather than fails.
///
/// Counted in the mono samples this returns, which is also what it holds:
/// the downmix happens per decoded frame, so there is no interleaved copy of
/// the whole track to bound separately. Twenty minutes at 48 kHz, which is
/// 230 MB of `f32` and far past any video anybody sends through a chat.
const MAX_DECODED_SAMPLES: usize = 48_000 * 1200;

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
#[cfg(all(test, not(target_family = "wasm")))]
fn wrap_aac_as_adts(frames: &[Vec<u8>], sample_rate: u32, channels: u8) -> Vec<u8> {
    let mut adts = Vec::new();
    for frame in frames {
        push_adts_frame(&mut adts, frame, sample_rate, channels);
    }
    adts
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::{MAX_ADTS_FRAME, MAX_DECODED_SAMPLES, wrap_aac_as_adts};

    /// The compressed ceiling is not the decoded one, and the gap between
    /// them is why both exist.
    ///
    /// Bounding the packet count bounds the metadata. It says nothing about
    /// what those packets expand into: an AAC packet is a few hundred bytes
    /// and decodes to 1024 samples a channel, so a track satisfying every
    /// bound on the compressed side still grows by three orders of magnitude.
    /// Written down as a test because this is the second ceiling that turned
    /// out to be measuring the wrong thing, and the next reader deserves the
    /// arithmetic rather than the conclusion.
    #[test]
    fn the_packet_ceiling_does_not_bound_what_the_packets_decode_to() {
        // What the packet ceiling alone would allow, at one AAC frame's worth
        // of stereo output per packet: two billion `f32`, which is eight
        // gigabytes and something like thirty-five times the decoded bound.
        let unbounded = super::super::demux::MAX_TRACK_SAMPLES * 1024 * 2;
        assert!(
            unbounded > MAX_DECODED_SAMPLES * 10,
            "the decoded ceiling has to be the binding one by a wide margin: \
             {unbounded} against {MAX_DECODED_SAMPLES}"
        );

        // And it is a size somebody can hold: well under a gigabyte of `f32`.
        let bytes = MAX_DECODED_SAMPLES * std::mem::size_of::<f32>();
        assert!(
            bytes < 512 * 1024 * 1024,
            "{bytes} bytes is too much to hold"
        );
    }

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
