//! Opus audio encoder for WhatsApp PTT messages.

/// Turning a prepared note into a voice note.
///
/// Opus, in an OGG container, which is what WhatsApp expects — and libopus is
/// C, so this whole module is absent from a build for the web, where the
/// browser's own encoder answers instead; see `crate::web::recorder`.
///
/// What arrives here is already resampled and already measured: the
/// resampling and the envelope are [`crate::PreparedNote`], which both
/// platforms share, so this module is the codec and nothing else.
#[cfg(not(target_family = "wasm"))]
mod opus_ogg {
    use log::info;
    use opus::{Application, Channels, Encoder};

    use super::EncoderError;
    use crate::ogg_opus::{FRAME_SIZE_SAMPLES, SAMPLE_RATE, needs_trailing_silence, package};
    use crate::recorder::PreparedNote;

    const CHANNELS: Channels = Channels::Mono;
    const BITRATE: i32 = 16000;

    pub fn encode_to_opus_ogg(note: &PreparedNote) -> Result<Vec<u8>, EncoderError> {
        let samples = &note.samples;
        if samples.is_empty() {
            return Err(EncoderError::EmptyAudio);
        }

        info!(
            "Encoding {} samples ({:.1}s) to Opus/OGG",
            samples.len(),
            samples.len() as f32 / SAMPLE_RATE as f32
        );

        let mut encoder = Encoder::new(SAMPLE_RATE, CHANNELS, Application::Voip)
            .map_err(|e| EncoderError::OpusError(e.to_string()))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(BITRATE))
            .map_err(|e| EncoderError::OpusError(e.to_string()))?;

        let samples_i16: Vec<i16> = samples
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
            .collect();

        let mut encoded_packets: Vec<Vec<u8>> = Vec::new();
        for chunk in samples_i16.chunks(FRAME_SIZE_SAMPLES) {
            let mut frame = chunk.to_vec();
            if frame.len() < FRAME_SIZE_SAMPLES {
                frame.resize(FRAME_SIZE_SAMPLES, 0);
            }

            let mut output = vec![0u8; 4000];
            let len = encoder
                .encode(&frame, &mut output)
                .map_err(|e| EncoderError::OpusError(e.to_string()))?;
            output.truncate(len);
            encoded_packets.push(output);
        }

        // When the final frame's zero-padding can't absorb the pre-skip (exact
        // frame multiples have none at all), one extra silence frame keeps the
        // packet stream covering the full logical duration.
        if needs_trailing_silence(encoded_packets.len(), samples_i16.len()) {
            let silence = vec![0i16; FRAME_SIZE_SAMPLES];
            let mut output = vec![0u8; 4000];
            let len = encoder
                .encode(&silence, &mut output)
                .map_err(|e| EncoderError::OpusError(e.to_string()))?;
            output.truncate(len);
            encoded_packets.push(output);
        }

        let ogg_buffer =
            package(encoded_packets, samples_i16.len()).map_err(EncoderError::OggError)?;

        info!("Encoded to {} bytes OGG", ogg_buffer.len());
        Ok(ogg_buffer)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        use crate::recorder::RecordedAudio;

        #[test]
        fn test_encode_simple_audio() {
            // Generate 1 second of silence
            let audio = RecordedAudio {
                samples: vec![0.0f32; 16000],
                sample_rate: 16000,
                duration_secs: 1,
            }
            .prepare();

            let result = encode_to_opus_ogg(&audio);
            assert!(result.is_ok());

            let ogg_data = result.unwrap();
            // Check OGG magic number
            assert_eq!(&ogg_data[0..4], b"OggS");
        }

        #[test]
        fn test_exact_frame_multiple_keeps_full_duration() {
            // 16000 samples = exactly 50 frames: no zero-padding to absorb the
            // pre-skip, so a capped EOS granule would clip ~6.5ms of real audio.
            let samples = 16000u64;
            let audio = RecordedAudio {
                samples: vec![0.0f32; samples as usize],
                sample_rate: 16000,
                duration_secs: 1,
            }
            .prepare();

            let ogg_data = encode_to_opus_ogg(&audio).unwrap();

            // The EOS granule lives in the header of the last OGG page
            // (byte offset 6, 8 bytes LE after the "OggS" capture pattern).
            let last_page = ogg_data
                .windows(4)
                .rposition(|w| w == b"OggS")
                .expect("no OGG page found");
            let granule_bytes: [u8; 8] =
                ogg_data[last_page + 6..last_page + 14].try_into().unwrap();
            let eos_granule = u64::from_le_bytes(granule_bytes);
            assert_eq!(eos_granule, crate::ogg_opus::eos_granule(samples as usize));
        }
    }
}

#[cfg(not(target_family = "wasm"))]
pub use opus_ogg::encode_to_opus_ogg;

/// The same call, on a platform with no encoder to make it with.
///
/// # Errors
///
/// Always. Nothing reaches it: a page's `stop` answers
/// [`Recording::Pending`](crate::Recording::Pending), whose codec is the
/// browser's, so no prepared note ever arrives here.
#[cfg(target_family = "wasm")]
pub fn encode_to_opus_ogg(_note: &crate::recorder::PreparedNote) -> Result<Vec<u8>, EncoderError> {
    Err(EncoderError::OpusError(
        "this build has no Opus encoder on the web".to_string(),
    ))
}

#[derive(Debug)]
pub enum EncoderError {
    EmptyAudio,
    OpusError(String),
    OggError(String),
}

impl std::fmt::Display for EncoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyAudio => write!(f, "No audio data to encode"),
            Self::OpusError(e) => write!(f, "Opus encoder error: {}", e),
            Self::OggError(e) => write!(f, "OGG writer error: {}", e),
        }
    }
}

impl std::error::Error for EncoderError {}
