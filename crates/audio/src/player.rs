//! Audio playback using cpal
//!
//! Plays Opus/OGG audio files for PTT voice message playback.

/// Playing a clip through a sound card, having decoded it here.
///
/// Both halves are unavailable to a page: cpal's WebAudio backend exists, but
/// libopus is C and there is no Opus decoder in this tree a browser can run —
/// which is moot, because a browser decodes Opus itself. The web backend in
/// `crate::web` is a real player rather than a stub; what it shares with this
/// one is [`PlayerError`], so the front end above them has one vocabulary.
#[cfg(not(target_family = "wasm"))]
mod cpal_output {
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
    use log::{error, info, warn};
    use ogg::reading::PacketReader;
    use opus::{Channels, Decoder as OpusDecoder};
    use tokio::sync::oneshot;

    use super::PlayerError;

    /// Audio player for PTT voice messages.
    pub struct AudioPlayer {
        stream: Option<Stream>,
        is_playing: Arc<AtomicBool>,
        position: Arc<AtomicUsize>,
        total_samples: u64,
        sample_rate: u32,
        /// Interleaved channels in the output stream.
        ///
        /// `position` and `total_samples` count *samples*, and the resampler
        /// writes one per channel per frame, so a second of stereo audio is two
        /// `sample_rate`s worth of them. Without this the clock ran at double
        /// speed on any stereo device and passed the clip's stated length.
        channels: usize,
        /// Whether the clip ran to its end.
        ///
        /// Distinct from `!is_playing`, which is also true while paused. The
        /// stream is still open after a natural completion — the callback only
        /// clears the flag — so rewinding `position` would set it playing again
        /// with nobody listening and no completion left to fire. Seeks are refused
        /// while this is set; the caller starts the clip over instead.
        finished: Arc<AtomicBool>,
        /// How fast the next clip is played, pitch preserved.
        ///
        /// Applied when the samples are prepared rather than by reading the stream
        /// faster: doubling the read rate doubles every frequency with it, and a
        /// voice note at 2× is meant to be the same voice, sooner.
        speed: f32,
        /// Input length over output length, for the clip that is queued.
        ///
        /// The clocks read the *queued* samples, so they have to be scaled by what
        /// the re-timing actually achieved rather than by what was requested: a
        /// video's audio is never stretched, and a clip too short to stretch comes
        /// back unchanged whatever was asked.
        time_scale: f32,
        completion_tx: Option<oneshot::Sender<()>>,
    }

    impl Default for AudioPlayer {
        fn default() -> Self {
            Self::new()
        }
    }

    impl AudioPlayer {
        pub fn new() -> Self {
            Self {
                stream: None,
                is_playing: Arc::new(AtomicBool::new(false)),
                position: Arc::new(AtomicUsize::new(0)),
                finished: Arc::new(AtomicBool::new(false)),
                channels: 1,
                total_samples: 0,
                sample_rate: 48000,
                speed: 1.0,
                time_scale: 1.0,
                completion_tx: None,
            }
        }

        /// Choose the speed for the next clip.
        ///
        /// Takes effect at the next [`play`](Self::play): the samples are re-timed
        /// once, up front, so nothing on the audio thread has to know about it.
        pub fn set_speed(&mut self, speed: f32) {
            self.speed = if speed.is_finite() && speed > 0.0 {
                speed.clamp(0.25, 4.0)
            } else {
                1.0
            };
        }

        pub fn speed(&self) -> f32 {
            self.speed
        }

        /// Returns a receiver that fires when playback completes.
        pub fn on_complete(&mut self) -> oneshot::Receiver<()> {
            let (tx, rx) = oneshot::channel();
            self.completion_tx = Some(tx);
            rx
        }

        pub fn is_playing(&self) -> bool {
            self.is_playing.load(Ordering::Relaxed)
        }

        /// How far through the clip playback is, in `0.0..=1.0`.
        ///
        /// Zero when nothing is loaded, so a caller can render a progress bar
        /// without first asking whether there is audio.
        pub fn progress(&self) -> f32 {
            if self.total_samples == 0 {
                return 0.0;
            }
            (self.position.load(Ordering::Relaxed) as f32 / self.total_samples as f32)
                .clamp(0.0, 1.0)
        }

        /// Jump to `fraction` of the way through, in `0.0..=1.0`.
        ///
        /// Playback continues from there rather than restarting: the callback
        /// reads this counter every buffer, so moving it is the whole seek. The
        /// counter is in interleaved samples, so the target is rounded down to a
        /// frame boundary — landing mid-frame on a stereo stream swaps the
        /// channels for the rest of the clip.
        pub fn seek(&self, fraction: f32) -> bool {
            if self.total_samples == 0 || self.finished.load(Ordering::Relaxed) {
                return false;
            }
            let channels = self.channels.max(1);
            let target = (fraction.clamp(0.0, 1.0) * self.total_samples as f32) as usize;
            let aligned = target - (target % channels);
            // One frame short of the end: landing exactly on it would let the
            // callback fire completion for a seek the user meant as "replay the
            // last moment".
            let last = (self.total_samples as usize).saturating_sub(channels);
            self.position.store(aligned.min(last), Ordering::Relaxed);
            true
        }

        /// Whether the loaded clip played to its end.
        pub fn is_finished(&self) -> bool {
            self.finished.load(Ordering::Relaxed)
        }

        /// Interleaved samples in one second of output.
        fn samples_per_sec(&self) -> usize {
            self.sample_rate as usize * self.channels.max(1)
        }

        /// Seconds of audio played so far, on the *clip's* clock.
        ///
        /// Scaled by the speed, so a note played at 2× still counts up to the
        /// duration printed beside it rather than to half of it.
        pub fn elapsed_secs(&self) -> f32 {
            let per_sec = self.samples_per_sec();
            if per_sec == 0 {
                return 0.0;
            }
            self.position.load(Ordering::Relaxed) as f32 / per_sec as f32 * self.time_scale
        }

        /// Total length in seconds, or zero when nothing is loaded.
        pub fn total_secs(&self) -> f32 {
            let per_sec = self.samples_per_sec();
            if per_sec == 0 {
                return 0.0;
            }
            self.total_samples as f32 / per_sec as f32 * self.time_scale
        }

        /// Play a voice note, at whatever speed the listener chose.
        pub fn play(&mut self, ogg_data: Vec<u8>) -> Result<(), PlayerError> {
            let samples = decode_ogg(&ogg_data)?;
            if samples.is_empty() {
                return Err(PlayerError::EmptyAudio);
            }

            info!("Decoded {} samples for playback", samples.len());
            let speed = self.speed;
            self.play_at(samples, 48000, speed)
        }

        /// Play raw f32 PCM samples at the specified sample rate.
        ///
        /// Always at 1×. This is a video's audio track, and the chosen speed
        /// belongs to voice notes: the frames play at normal speed either way, so
        /// a 2× left over from a voice note made the sound race the picture and
        /// finish early. Enforced here rather than remembered at each call site,
        /// which is what it was.
        pub fn play_samples(
            &mut self,
            samples: Vec<f32>,
            src_sample_rate: u32,
        ) -> Result<(), PlayerError> {
            self.play_at(samples, src_sample_rate, 1.0)
        }

        fn play_at(
            &mut self,
            samples: Vec<f32>,
            src_sample_rate: u32,
            speed: f32,
        ) -> Result<(), PlayerError> {
            // Preserve completion sender through stop() since it may have been set by on_complete()
            let saved_completion_tx = self.completion_tx.take();
            self.stop();
            self.completion_tx = saved_completion_tx;

            if samples.is_empty() {
                return Err(PlayerError::EmptyAudio);
            }

            info!(
                "Playing {} samples at {} Hz",
                samples.len(),
                src_sample_rate
            );

            let host = cpal::default_host();
            let device = host
                .default_output_device()
                .ok_or(PlayerError::NoOutputDevice)?;

            info!("Using default output device");

            // Prefer F32 (native to our samples), but i16/u16-only devices still
            // play: the callback converts per sample. Formats build_stream can't
            // dispatch (i32/f64/...) are filtered out up front so a fallback pick
            // never lands on one while a buildable range exists.
            let supported_configs: Vec<_> = device
                .supported_output_configs()
                .map_err(|e| PlayerError::DeviceError(e.to_string()))?
                .filter(|c| {
                    matches!(
                        c.sample_format(),
                        SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
                    )
                })
                .collect();

            let supports_48k = |c: &cpal::SupportedStreamConfigRange| {
                c.min_sample_rate() <= 48000 && c.max_sample_rate() >= 48000
            };
            let is_f32 =
                |c: &cpal::SupportedStreamConfigRange| c.sample_format() == SampleFormat::F32;
            let chosen = supported_configs
                .iter()
                .find(|c| is_f32(c) && supports_48k(c))
                .or_else(|| supported_configs.iter().find(|c| is_f32(c)))
                .or_else(|| supported_configs.iter().find(|c| supports_48k(c)))
                .or_else(|| supported_configs.first())
                .ok_or(PlayerError::NoSupportedConfig)?;

            let sample_format = chosen.sample_format();
            let config: StreamConfig = if supports_48k(chosen) {
                chosen.with_sample_rate(48000)
            } else {
                chosen.with_sample_rate(chosen.min_sample_rate())
            }
            .into();
            self.sample_rate = config.sample_rate;
            let output_channels = config.channels as usize;
            self.channels = output_channels.max(1);

            info!(
                "Output config: {} Hz, {} channels, {:?}",
                config.sample_rate, output_channels, sample_format
            );

            let resampled =
                resample_audio(&samples, src_sample_rate, self.sample_rate, output_channels);
            let before = resampled.len();
            let resampled = crate::timescale::stretch(resampled, output_channels, speed);
            // The ratio achieved, not the one asked for. `stretch` returns a clip
            // shorter than one frame unchanged, and both clocks are derived from
            // the samples that are actually queued — so a short note at 2× was
            // counting up to twice its own length.
            self.time_scale = if resampled.is_empty() {
                1.0
            } else {
                before as f32 / resampled.len() as f32
            };
            self.total_samples = resampled.len() as u64;

            let is_playing = self.is_playing.clone();
            let position = self.position.clone();
            position.store(0, Ordering::Relaxed);
            let finished = self.finished.clone();
            finished.store(false, Ordering::Relaxed);

            let completion_tx: Arc<Mutex<Option<oneshot::Sender<()>>>> =
                Arc::new(Mutex::new(self.completion_tx.take()));
            // Kept out of the closures so a failed start can hand the sender back
            // instead of stranding whoever is awaiting completion.
            let completion_handle = completion_tx.clone();
            let audio_data = Arc::new(resampled);

            let stream = match sample_format {
                SampleFormat::F32 => build_stream::<f32>(
                    &device,
                    config,
                    audio_data,
                    position,
                    is_playing,
                    finished,
                    completion_tx,
                ),
                SampleFormat::I16 => build_stream::<i16>(
                    &device,
                    config,
                    audio_data,
                    position,
                    is_playing,
                    finished,
                    completion_tx,
                ),
                SampleFormat::U16 => build_stream::<u16>(
                    &device,
                    config,
                    audio_data,
                    position,
                    is_playing,
                    finished,
                    completion_tx,
                ),
                other => Err(PlayerError::StreamError(format!(
                    "unsupported output sample format {other:?}"
                ))),
            };
            let stream = match stream {
                Ok(stream) => stream,
                Err(e) => {
                    self.completion_tx = completion_handle.lock().ok().and_then(|mut g| g.take());
                    return Err(e);
                }
            };

            // Only now: the callback reads `is_playing` to decide whether it still
            // owes a completion, and it cannot run before `play()` succeeds. Set it
            // earlier and a failed start leaves the player claiming to play.
            self.is_playing.store(true, Ordering::Relaxed);
            if let Err(e) = stream.play() {
                self.is_playing.store(false, Ordering::Relaxed);
                self.completion_tx = completion_handle.lock().ok().and_then(|mut g| g.take());
                return Err(PlayerError::StreamError(e.to_string()));
            }

            self.stream = Some(stream);
            info!("Audio playback started");

            Ok(())
        }

        pub fn stop(&mut self) {
            self.stream.take();
            self.is_playing.store(false, Ordering::Relaxed);
            self.finished.store(false, Ordering::Relaxed);
            self.position.store(0, Ordering::Relaxed);
            self.total_samples = 0;
            self.channels = 1;
            self.completion_tx = None;
        }

        /// Pause the stream. The flag follows the device: reporting "paused" for a
        /// stream that refused to pause would leave the UI describing audio the
        /// user can still hear.
        pub fn pause(&mut self) {
            if let Some(ref stream) = self.stream {
                match stream.pause() {
                    Ok(()) => self.is_playing.store(false, Ordering::Relaxed),
                    Err(e) => error!("Audio pause failed: {e}"),
                }
            }
        }

        /// Resume the stream. See [`pause`](Self::pause) for why the flag only
        /// moves on success.
        pub fn resume(&mut self) {
            if let Some(ref stream) = self.stream {
                match stream.play() {
                    Ok(()) => self.is_playing.store(true, Ordering::Relaxed),
                    Err(e) => error!("Audio resume failed: {e}"),
                }
            }
        }
    }

    /// Build the output stream for the device's sample format, converting our
    /// f32 samples in the callback (same dispatch as call_device's speaker path).
    fn build_stream<T: SizedSample + FromSample<f32>>(
        device: &cpal::Device,
        config: StreamConfig,
        audio: Arc<Vec<f32>>,
        position: Arc<AtomicUsize>,
        is_playing: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
        completion_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    ) -> Result<Stream, PlayerError> {
        let err_is_playing = is_playing.clone();
        let err_finished = finished.clone();
        let err_completion = completion_tx.clone();
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    let mut pos = position.load(Ordering::Relaxed);

                    for sample in data.iter_mut() {
                        let s = if pos < audio.len() {
                            let s = audio[pos];
                            pos += 1;
                            s
                        } else {
                            // Mark as done and notify completion (only once)
                            if is_playing.swap(false, Ordering::Relaxed) {
                                finished.store(true, Ordering::Relaxed);
                                if let Ok(mut guard) = completion_tx.lock()
                                    && let Some(tx) = guard.take()
                                {
                                    let _ = tx.send(());
                                }
                            }
                            0.0
                        };
                        *sample = T::from_sample(s);
                    }

                    position.store(pos, Ordering::Relaxed);
                },
                move |err| {
                    error!("Audio output error: {}", err);
                    // A dead stream produces no further callbacks, so clearing the
                    // flag and settling the completion here is the only thing that
                    // keeps a waiter from hanging on audio that stopped.
                    if err_is_playing.swap(false, Ordering::Relaxed) {
                        err_finished.store(true, Ordering::Relaxed);
                        if let Ok(mut guard) = err_completion.lock()
                            && let Some(tx) = guard.take()
                        {
                            let _ = tx.send(());
                        }
                    }
                },
                None,
            )
            .map_err(|e| PlayerError::StreamError(e.to_string()))
    }

    fn decode_ogg(ogg_data: &[u8]) -> Result<Vec<f32>, PlayerError> {
        let cursor = Cursor::new(ogg_data);
        let mut packet_reader = PacketReader::new(cursor);
        let mut all_samples: Vec<f32> = Vec::new();
        let mut packet_count = 0;
        let mut decoder: Option<OpusDecoder> = None;
        let mut channel_count = 1usize;
        let mut pre_skip = 0usize;

        while let Some(packet) = packet_reader
            .read_packet()
            .map_err(|e| PlayerError::DecodeError(format!("OGG read error: {}", e)))?
        {
            packet_count += 1;

            // Header packets are identified by signature, not by ordinal
            // position. Skipping packets 1 and 2 unconditionally would eat the
            // first two audio packets of a stream that omits OpusHead — exactly
            // the malformed-but-playable case the fallback below exists for.
            if packet.data.starts_with(b"OpusHead") {
                if packet.data.len() >= 12 {
                    let channels = packet.data[9];
                    // 48kHz priming samples the decoder must discard before real audio
                    pre_skip = u16::from_le_bytes([packet.data[10], packet.data[11]]) as usize;
                    channel_count = if channels > 1 { 2 } else { 1 };
                    let opus_channels = if channel_count == 2 {
                        Channels::Stereo
                    } else {
                        Channels::Mono
                    };
                    decoder = Some(OpusDecoder::new(48000, opus_channels).map_err(|e| {
                        PlayerError::DecodeError(format!("Opus decoder init: {}", e))
                    })?);
                }
                continue;
            }
            if packet.data.starts_with(b"OpusTags") {
                continue;
            }

            // Some malformed-but-playable streams omit OpusHead.
            if decoder.is_none() {
                decoder =
                    Some(OpusDecoder::new(48000, Channels::Mono).map_err(|e| {
                        PlayerError::DecodeError(format!("Opus decoder init: {e}"))
                    })?);
            }
            let Some(dec) = decoder.as_mut() else {
                return Err(PlayerError::DecodeError(
                    "Opus decoder was not initialized".to_string(),
                ));
            };

            let mut output = vec![0.0f32; 5760 * 2];
            match dec.decode_float(&packet.data, &mut output, false) {
                Ok(n) => {
                    // n is frames per channel; the buffer is interleaved
                    output.truncate(n * channel_count);
                    let mono = if channel_count == 2 {
                        output
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|pair| (pair[0] + pair[1]) / 2.0)
                            .collect()
                    } else {
                        output
                    };
                    let skip = pre_skip.min(mono.len());
                    pre_skip -= skip;
                    all_samples.extend_from_slice(&mono[skip..]);
                }
                Err(e) => warn!("Opus decode error (packet {}): {}", packet_count, e),
            }
        }

        info!(
            "Decoded {} packets, {} samples",
            packet_count,
            all_samples.len()
        );

        if all_samples.is_empty() {
            return Err(PlayerError::DecodeError("No samples decoded".to_string()));
        }

        Ok(all_samples)
    }

    fn resample_audio(samples: &[f32], src_rate: u32, dst_rate: u32, channels: usize) -> Vec<f32> {
        if src_rate == 0 || dst_rate == 0 {
            return samples.to_vec();
        }

        if src_rate == dst_rate && channels == 1 {
            return samples.to_vec();
        }

        let ratio = dst_rate as f32 / src_rate as f32;
        let output_len = (samples.len() as f32 * ratio) as usize;
        let mut output = Vec::with_capacity(output_len * channels);

        for i in 0..output_len {
            let src_idx = (i as f32 / ratio) as usize;
            let sample = samples.get(src_idx).copied().unwrap_or(0.0);
            output.extend(std::iter::repeat_n(sample, channels));
        }

        output
    }

    #[cfg(test)]
    mod tests {

        /// The clocks read the samples that are queued, so they have to be scaled
        /// by what the re-timing achieved rather than by what was asked for. A
        /// clip too short to stretch comes back unchanged, and a video's audio is
        /// never stretched at all — both were being counted as if they had been.
        #[test]
        fn the_clock_follows_the_samples_that_were_queued() {
            let mut player = AudioPlayer::default();
            player.set_speed(2.0);
            assert_eq!(player.speed(), 2.0);

            // Nothing has been prepared, so nothing has been re-timed.
            assert_eq!(player.total_secs(), 0.0);
            assert_eq!(player.elapsed_secs(), 0.0);
        }
        use super::*;

        /// A player with nothing loaded is the state every caller sees first, and
        /// the one where a naive `position / total` divides by zero.
        #[test]
        fn an_idle_player_reports_zero_rather_than_dividing_by_zero() {
            let player = AudioPlayer::new();
            assert_eq!(player.progress(), 0.0);
            assert_eq!(player.elapsed_secs(), 0.0);
            assert_eq!(player.total_secs(), 0.0);
        }

        #[test]
        fn seeking_with_nothing_loaded_does_nothing() {
            let player = AudioPlayer::new();
            player.seek(0.5);
            assert_eq!(player.progress(), 0.0);
        }

        #[test]
        fn seek_maps_a_fraction_onto_the_clip() {
            let mut player = AudioPlayer::new();
            player.total_samples = 1000;
            player.sample_rate = 100;

            player.seek(0.25);
            assert_eq!(player.position.load(Ordering::Relaxed), 250);
            assert!((player.progress() - 0.25).abs() < f32::EPSILON);
            assert!((player.elapsed_secs() - 2.5).abs() < f32::EPSILON);
            assert!((player.total_secs() - 10.0).abs() < f32::EPSILON);
        }

        #[test]
        fn seeking_to_the_very_end_stays_inside_the_clip() {
            // Landing exactly on the end would let the callback fire completion
            // for a scrub the user meant as "replay the last moment".
            let mut player = AudioPlayer::new();
            player.total_samples = 1000;

            player.seek(1.0);
            assert_eq!(player.position.load(Ordering::Relaxed), 999);
        }

        #[test]
        fn out_of_range_fractions_are_clamped() {
            let mut player = AudioPlayer::new();
            player.total_samples = 1000;

            player.seek(-3.0);
            assert_eq!(player.position.load(Ordering::Relaxed), 0);

            player.seek(7.5);
            assert_eq!(player.position.load(Ordering::Relaxed), 999);
        }

        /// The resampler writes one sample per channel per frame, so a stereo
        /// clip holds twice the samples of the same clip in mono. Dividing by the
        /// sample rate alone ran the clock at double speed and took the elapsed
        /// count past the duration the message advertised.
        #[test]
        fn a_stereo_clip_lasts_as_long_as_the_same_clip_in_mono() {
            let mut mono = AudioPlayer::new();
            mono.total_samples = 1000;
            mono.sample_rate = 100;
            mono.channels = 1;

            let mut stereo = AudioPlayer::new();
            stereo.total_samples = 2000;
            stereo.sample_rate = 100;
            stereo.channels = 2;

            assert!((mono.total_secs() - 10.0).abs() < f32::EPSILON);
            assert!(
                (stereo.total_secs() - 10.0).abs() < f32::EPSILON,
                "ten seconds of audio is ten seconds however many channels carry it"
            );

            stereo.seek(0.25);
            assert!((stereo.elapsed_secs() - 2.5).abs() < f32::EPSILON);
        }

        /// A seek that lands between the left and right sample of one frame
        /// swaps the channels for the rest of the clip.
        #[test]
        fn a_seek_lands_on_a_frame_boundary() {
            let mut player = AudioPlayer::new();
            player.total_samples = 1000;
            player.sample_rate = 100;
            player.channels = 2;

            // 0.3335 * 1000 = 333 samples, which is mid-frame.
            player.seek(0.3335);
            let position = player.position.load(Ordering::Relaxed);
            assert_eq!(position % 2, 0, "a stereo position is a whole frame");
            assert_eq!(position, 332);

            player.seek(1.0);
            let end = player.position.load(Ordering::Relaxed);
            assert_eq!(end % 2, 0);
            assert_eq!(end, 998, "one whole frame short of the end");
        }
    }
}

#[cfg(not(target_family = "wasm"))]
pub use cpal_output::AudioPlayer;

#[derive(Debug)]
pub enum PlayerError {
    NoOutputDevice,
    NoSupportedConfig,
    EmptyAudio,
    DeviceError(String),
    StreamError(String),
    DecodeError(String),
}

impl std::fmt::Display for PlayerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoOutputDevice => write!(f, "No audio output device found"),
            Self::NoSupportedConfig => write!(f, "No supported audio configuration"),
            Self::EmptyAudio => write!(f, "No audio data to play"),
            Self::DeviceError(e) => write!(f, "Audio device error: {}", e),
            Self::StreamError(e) => write!(f, "Audio stream error: {}", e),
            Self::DecodeError(e) => write!(f, "Audio decode error: {}", e),
        }
    }
}

impl std::error::Error for PlayerError {}
