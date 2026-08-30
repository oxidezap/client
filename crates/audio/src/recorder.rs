//! Audio recording using cpal
//!
//! Captures audio from the default input device at 48kHz mono.
//! The samples are stored and can be resampled to 16kHz for Opus encoding.

/// Target sample rate for Opus encoding (WhatsApp standard)
pub const TARGET_SAMPLE_RATE: u32 = 16000;

/// Capture sample rate (most hardware supports this)
#[cfg(not(target_family = "wasm"))]
const CAPTURE_SAMPLE_RATE: u32 = 48000;

/// Longest voice note either platform will hold.
///
/// Ten minutes, past which the capture stops growing rather than growing
/// until something is killed. At a capture rate of 48 kHz that is already 115
/// MB of `f32`, which is far more than anybody records by mistake, and on
/// the web it is a linear memory with a ceiling, where running out is an
/// abort rather than an error.
pub(crate) const MAX_RECORDING_SECS: usize = 600;

/// That ceiling in samples, at the rate the device is actually running.
///
/// Derived rather than fixed: a microphone that will not do 48 kHz is opened
/// at its best rate, and a count of samples is a length of time only against
/// the rate producing them. Fixed at 48 kHz, a 96 kHz device stopped
/// capturing after five minutes while `stop` went on reporting the wall clock
///, a voice note truncated in silence, claiming a duration longer than its
/// audio.
pub(crate) fn max_recording_samples(sample_rate: u32) -> usize {
    sample_rate as usize * MAX_RECORDING_SECS
}

pub struct RecordedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub duration_secs: u32,
}

/// A voice note that is already encoded, and everything drawn from it.
///
/// The waveform travels with the bytes rather than being derived from them:
/// it is measured from the samples while they are still samples, and once a
/// platform has handed back an encoded note there is nothing left to measure.
pub struct EncodedNote {
    pub bytes: Vec<u8>,
    pub waveform: Vec<u8>,
    pub duration_secs: u32,
}

/// What stopping a recording produced.
///
/// Two shapes because the platforms answer at different times. A desktop hands
/// back samples and the encode is ordinary work on a background thread; a
/// browser encodes *as it captures*, through an encoder whose last packets
/// arrive after the microphone has closed, so the answer is a channel rather
/// than a value.
///
/// The caller awaits one and encodes the other, which is the whole difference
/// and is why this is an enum rather than a future on both: making the desktop
/// asynchronous to match would move a real encode off the background pool for
/// nothing.
pub enum Recording {
    /// Samples this build encodes itself.
    Samples(RecordedAudio),
    /// A note the platform is still flushing.
    Pending(futures_channel::oneshot::Receiver<Result<EncodedNote, RecorderError>>),
}

impl RecordedAudio {
    pub fn resample_to_16khz(&self) -> Vec<f32> {
        if self.sample_rate == TARGET_SAMPLE_RATE {
            return self.samples.clone();
        }

        let ratio = self.sample_rate as f32 / TARGET_SAMPLE_RATE as f32;
        let output_len = (self.samples.len() as f32 / ratio) as usize;
        let mut output = Vec::with_capacity(output_len);

        if self.sample_rate.is_multiple_of(TARGET_SAMPLE_RATE) {
            // Integer decimation (the common 48kHz case): low-pass BEFORE
            // dropping samples, or everything above the 8kHz target Nyquist
            // folds back into the voice band (a box average only manages
            // ~10dB there). Windowed-sinc FIR, ~7kHz cutoff at the input
            // rate, unity DC gain so speech level is preserved; evaluated
            // only at the kept samples, O(n·taps) — fine for PTT lengths.
            const CUTOFF_HZ: f32 = 7_000.0;
            let step = (self.sample_rate / TARGET_SAMPLE_RATE) as usize;
            // The transition band scales with the input rate, so the tap
            // count must too: 63 taps suit 48kHz (step 3), but a 96/192kHz
            // fallback device needs proportionally more or content above
            // 8kHz still folds into the output.
            let taps = (21 * step) | 1;
            let fc = CUTOFF_HZ / self.sample_rate as f32;
            let center = (taps - 1) / 2;
            let mut fir = vec![0.0f32; taps];
            for (k, tap) in fir.iter_mut().enumerate() {
                let n = k as f32 - center as f32;
                let sinc = if n == 0.0 {
                    2.0 * fc
                } else {
                    (std::f32::consts::TAU * fc * n).sin() / (std::f32::consts::PI * n)
                };
                let hamming =
                    0.54 - 0.46 * (std::f32::consts::TAU * k as f32 / (taps - 1) as f32).cos();
                *tap = sinc * hamming;
            }
            let dc_gain: f32 = fir.iter().sum();
            for tap in fir.iter_mut() {
                *tap /= dc_gain;
            }
            // saturating: an empty capture must not underflow (the loop below
            // is a no-op then anyway).
            let last = self.samples.len().saturating_sub(1);
            for i in 0..self.samples.len() / step {
                let mid = (i * step) as isize;
                let mut acc = 0.0f32;
                for (k, &tap) in fir.iter().enumerate() {
                    // Clamped edges: replicating the boundary sample beats
                    // zero-padding, which would fade the clip's ends.
                    let src = (mid + k as isize - center as isize).clamp(0, last as isize);
                    acc += tap * self.samples[src as usize];
                }
                output.push(acc);
            }
        } else {
            for i in 0..output_len {
                // Linear interpolation: nearest-neighbor sample dropping
                // aliases audibly on voice.
                let src_pos = i as f32 * ratio;
                let idx = src_pos as usize;
                let frac = src_pos - idx as f32;
                let Some(&a) = self.samples.get(idx) else {
                    break;
                };
                let b = self.samples.get(idx + 1).copied().unwrap_or(a);
                output.push(a + (b - a) * frac);
            }
        }

        output
    }
}

/// Opening a microphone, which is a thing only an operating system has.
///
/// Gathered into one module so the recording that a page cannot do is absent
/// rather than stubbed line by line, and so the types above it —
/// [`RecordedAudio`], [`RecorderError`] — stay shared: the web backend
/// answers in exactly the same vocabulary.
#[cfg(not(target_family = "wasm"))]
mod capture {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{Device, FromSample, Sample as _, SampleFormat, SizedSample, Stream, StreamConfig};
    use log::{error, info, warn};
    use ringbuf::HeapRb;
    use ringbuf::traits::Split as _;
    use wacore::time::Instant;

    use super::{
        CAPTURE_SAMPLE_RATE, RecordedAudio, RecorderError, Recording, max_recording_samples,
    };

    /// How much of the tail the meter averages: 150ms.
    /// Short enough to follow speech, long enough not to flicker.
    const LEVEL_WINDOW_MS: usize = 150;

    /// That window in samples, at the rate the device actually opened at.
    ///
    /// Not at `CAPTURE_SAMPLE_RATE`: a device that cannot do 48 kHz is opened
    /// at its own best rate instead (`with_max_sample_rate`), so a fixed
    /// count is 450ms of tail on a 16 kHz microphone. The meter then lags the
    /// voice with no symptom beyond looking dead.
    fn level_window(sample_rate: u32) -> usize {
        (sample_rate as usize * LEVEL_WINDOW_MS) / 1000
    }

    /// Root-mean-square of a slice, scaled so ordinary speech lands mid-meter.
    ///
    /// The bare RMS of a voice sits around 0.05–0.2, which would leave the meter
    /// looking dead; the gain is the difference between a meter and a decoration.
    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum: f32 = samples.iter().map(|s| s * s).sum();
        ((sum / samples.len() as f32).sqrt() * LEVEL_GAIN).clamp(0.0, 1.0)
    }

    /// Chosen so a normal speaking voice fills roughly half the meter.
    const LEVEL_GAIN: f32 = 4.0;

    /// How long the ring holds captured audio before the drain has to have
    /// taken it: two seconds, which is two hundred callbacks at the usual
    /// block size. Bounded because a ring is, and generous because losing
    /// samples here is losing part of what somebody said.
    const RING_SECS: usize = 2;

    /// That window in samples, at the rate the device actually opened at.
    ///
    /// Derived for the reason [`level_window`] is: fixed at 48 kHz, a 96 kHz
    /// fallback device gets one second of slack and a 192 kHz one gets half,
    /// and a drain delayed past that drops microphone samples on the floor.
    fn ring_samples(sample_rate: u32) -> usize {
        sample_rate as usize * RING_SECS
    }

    /// How often the drain empties the ring. Short against `RING_SECS`, so
    /// a scheduling hiccup on this side costs nothing.
    const DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

    pub struct AudioRecorder {
        stream: Option<Stream>,
        samples: Arc<Mutex<Vec<f32>>>,
        /// The meter's value, written by the drain and read by the interface.
        /// Bits of an `f32`, because the meter is one number and a lock for it
        /// would be the UI thread and the drain taking turns.
        level: Arc<std::sync::atomic::AtomicU32>,
        /// What ends the drain thread, and the handle to wait on.
        draining: Option<(
            Arc<std::sync::atomic::AtomicBool>,
            std::thread::JoinHandle<()>,
        )>,
        is_recording: bool,
        start_time: Option<Instant>,
        device: Option<Device>,
        config: Option<StreamConfig>,
        sample_format: SampleFormat,
        sample_rate: u32,
    }

    impl Default for AudioRecorder {
        fn default() -> Self {
            Self::new()
        }
    }

    impl AudioRecorder {
        pub fn new() -> Self {
            Self {
                stream: None,
                samples: Arc::new(Mutex::new(Vec::new())),
                level: Arc::new(std::sync::atomic::AtomicU32::new(0)),
                draining: None,
                is_recording: false,
                start_time: None,
                device: None,
                config: None,
                sample_format: SampleFormat::F32,
                sample_rate: CAPTURE_SAMPLE_RATE,
            }
        }

        pub fn init(&mut self) -> Result<(), RecorderError> {
            let host = cpal::default_host();

            let device = host
                .default_input_device()
                .ok_or(RecorderError::NoInputDevice)?;

            info!("Using default input device");

            let supported = device
                .supported_input_configs()
                .map_err(|e| RecorderError::DeviceError(e.to_string()))?;

            // Prefer F32 (native to our buffer), but i16/u16-only mics still
            // record: the callback converts per sample. Format outranks 48kHz
            // support, which outranks mono: multichannel is downmixed anyway,
            // while a low capture rate permanently costs voice bandwidth.
            let mut best: Option<(u8, _)> = None;
            for config in supported {
                if !matches!(
                    config.sample_format(),
                    SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
                ) {
                    continue;
                }
                let supports_rate = config.min_sample_rate() <= CAPTURE_SAMPLE_RATE
                    && config.max_sample_rate() >= CAPTURE_SAMPLE_RATE;
                // Capture rate outranks sample format: every accepted format is
                // converted to f32 anyway, so preferring an F32 config that cannot
                // reach CAPTURE_SAMPLE_RATE would throw away voice bandwidth for
                // nothing.
                let score = u8::from(supports_rate) * 4
                    + u8::from(config.sample_format() == SampleFormat::F32) * 2
                    + u8::from(config.channels() == 1);
                if best.as_ref().is_none_or(|(s, _)| score > *s) {
                    let candidate = if supports_rate {
                        config.with_sample_rate(CAPTURE_SAMPLE_RATE)
                    } else {
                        config.with_max_sample_rate()
                    };
                    best = Some((score, candidate));
                }
            }

            let supported_config = best
                .map(|(_, c)| c)
                .ok_or(RecorderError::NoSupportedConfig)?;

            self.sample_format = supported_config.sample_format();
            let stream_config: StreamConfig = supported_config.into();
            self.sample_rate = stream_config.sample_rate;

            info!(
                "Audio config: {} Hz, {} channel(s), {:?}",
                stream_config.sample_rate, stream_config.channels, self.sample_format
            );

            self.device = Some(device);
            self.config = Some(stream_config);

            Ok(())
        }

        pub fn start(&mut self) -> Result<(), RecorderError> {
            if self.is_recording {
                return Err(RecorderError::AlreadyRecording);
            }

            if self.device.is_none() {
                self.init()?;
            }

            let device = self.device.as_ref().ok_or(RecorderError::NotInitialized)?;
            let config = self.config.ok_or(RecorderError::NotInitialized)?;

            if let Ok(mut samples) = self.samples.lock() {
                samples.clear();
            }
            self.level.store(0, Ordering::Relaxed);

            // The callback writes into a lock-free ring and nothing else; a
            // thread on this side is what turns that into the capture. See
            // `spawn_drain`.
            let (producer, consumer) = HeapRb::<f32>::new(ring_samples(self.sample_rate)).split();

            let stream = match self.sample_format {
                SampleFormat::F32 => build_input_stream::<f32, _>(device, config, producer),
                SampleFormat::I16 => build_input_stream::<i16, _>(device, config, producer),
                SampleFormat::U16 => build_input_stream::<u16, _>(device, config, producer),
                other => Err(RecorderError::StreamError(format!(
                    "unsupported input sample format {other:?}"
                ))),
            }?;
            // After the device is running, not before: a `play` that fails —
            // the microphone unplugged between opening it and starting it —
            // returns from here, and a drain started above it would be a
            // thread nobody ever asks to stop, holding its ring, with the
            // next attempt overwriting the handle that could have joined it.
            stream
                .play()
                .inspect_err(|_| {
                    self.device = None;
                    self.config = None;
                })
                .map_err(|e| RecorderError::StreamError(e.to_string()))?;

            self.draining = Some(spawn_drain(
                consumer,
                self.samples.clone(),
                self.level.clone(),
                // The device's own rate, not the one asked for: a microphone
                // that will not do 48 kHz is opened at its best rate instead,
                // and a window in samples is a window in milliseconds only
                // against the rate it is actually capturing at.
                level_window(self.sample_rate),
                // And the ceiling in samples, for the same reason.
                max_recording_samples(self.sample_rate),
            ));

            self.stream = Some(stream);
            self.is_recording = true;
            self.start_time = Some(Instant::now());

            info!("Recording started");
            Ok(())
        }

        /// The input level right now, 0..=1, for a meter.
        ///
        /// Read off what has been captured rather than from a second tap on
        /// the device: a meter that opened its own stream would be a second
        /// consumer of a microphone the user only agreed to share once. One
        /// atomic, because this is called from the interface on every tick
        /// and taking the capture's lock for it put the UI thread in front of
        /// the audio thread.
        ///
        /// RMS, not peak: a peak meter is pinned by a single click and says
        /// nothing about whether a voice is being picked up.
        pub fn level(&self) -> f32 {
            f32::from_bits(self.level.load(Ordering::Relaxed))
        }

        /// Samples, always: this build has an encoder of its own, so there is
        /// nothing to wait for. See [`Recording`].
        pub fn stop(&mut self) -> Result<Recording, RecorderError> {
            if !self.is_recording {
                return Err(RecorderError::NotRecording);
            }

            // The device first, so nothing more is captured, then the drain,
            // which is what puts the last of the ring into the capture: read
            // before it has finished is a note missing its final moments.
            self.stream.take();
            self.stop_draining();
            self.is_recording = false;

            let samples = self.samples.lock().map(|b| b.clone()).unwrap_or_default();
            // From the samples that were kept, never from the wall clock. The
            // drain stops appending at `max_recording_samples`, so a note left
            // running past that would otherwise advertise a length its audio
            // does not have: ten minutes of sound claiming twenty, with the
            // player's scrubber over silence that is not there.
            let duration =
                Duration::from_secs_f64(samples.len() as f64 / f64::from(self.sample_rate.max(1)));

            info!(
                "Recording stopped: {} samples, {:.1}s",
                samples.len(),
                duration.as_secs_f32()
            );

            Ok(Recording::Samples(RecordedAudio {
                samples,
                sample_rate: self.sample_rate,
                duration_secs: duration.as_secs() as u32,
            }))
        }

        pub fn cancel(&mut self) {
            self.stream.take();
            self.stop_draining();
            self.is_recording = false;
            self.start_time = None;
            if let Ok(mut samples) = self.samples.lock() {
                samples.clear();
            }
            warn!("Recording cancelled");
        }

        /// Ask the drain to finish what is in the ring and wait for it.
        fn stop_draining(&mut self) {
            let Some((stop, handle)) = self.draining.take() else {
                return;
            };
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    }

    impl Drop for AudioRecorder {
        /// A recorder that is simply let go still has a thread in it.
        ///
        /// The drain leaves only on its stop flag: dropping the stream drops
        /// the ring's producer, which the loop does not watch, so it goes on
        /// waking every 20 ms for ever, holding the ring and the capture. The
        /// two ways out of a recording -- `stop` and `cancel` -- both raise
        /// the flag, and this is the third: a window closed mid-recording, or
        /// a `RecorderError` on a path that returns the recorder rather than
        /// cancelling it.
        fn drop(&mut self) {
            // Before the drain, for the reason `stop` does it in this order:
            // nothing more is captured, and what is in the ring is drained
            // rather than abandoned.
            self.stream.take();
            self.stop_draining();
        }
    }

    /// Turn what the callback pushed into the capture, off the audio thread.
    ///
    /// This is where everything the realtime callback must not do happens: the
    /// lock the interface also takes, a `Vec` that grows, and the meter's
    /// arithmetic. The callback's whole job is a downmix and a lock-free
    /// push, which is what the call device next door has always done.
    fn spawn_drain(
        mut consumer: impl ringbuf::traits::Consumer<Item = f32> + Send + 'static,
        capture: Arc<Mutex<Vec<f32>>>,
        level: Arc<std::sync::atomic::AtomicU32>,
        window: usize,
        ceiling: usize,
    ) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let stop = Arc::new(AtomicBool::new(false));
        let ending = stop.clone();
        let handle = std::thread::Builder::new()
            .name("oxidezap-recorder-drain".to_string())
            .spawn(move || {
                let mut block = vec![0.0f32; ring_samples(CAPTURE_SAMPLE_RATE)];
                // The meter's window, kept here so reading it never touches
                // the capture's lock.
                let mut tail: std::collections::VecDeque<f32> =
                    std::collections::VecDeque::with_capacity(window);
                let mut full = false;
                loop {
                    let ending_now = ending.load(Ordering::Relaxed);
                    let taken = consumer.pop_slice(&mut block);
                    if taken > 0 {
                        let taken = &block[..taken];
                        for &sample in taken {
                            if tail.len() == window {
                                tail.pop_front();
                            }
                            tail.push_back(sample);
                        }
                        level.store(rms_of(&mut tail).to_bits(), Ordering::Relaxed);
                        if let Ok(mut samples) = capture.lock() {
                            let room = ceiling.saturating_sub(samples.len());
                            if room < taken.len() && !full {
                                full = true;
                                warn!("recording reached its ceiling; capturing no more");
                            }
                            samples.extend_from_slice(&taken[..room.min(taken.len())]);
                        }
                    }
                    // Asked *before* the pop above, so the last block pushed
                    // before the stream was taken is still drained: reading
                    // the flag after would leave whatever arrived in between.
                    if ending_now && taken == 0 {
                        return;
                    }
                    if taken == 0 {
                        std::thread::sleep(DRAIN_INTERVAL);
                    }
                }
            })
            .expect("a thread for the recorder's drain");
        (stop, handle)
    }

    /// [`rms`] over the meter's window, which is a ring rather than a slice.
    fn rms_of(tail: &mut std::collections::VecDeque<f32>) -> f32 {
        rms(tail.make_contiguous())
    }

    /// Build the input stream for the device's sample format, converting to f32
    /// in the callback (same dispatch as the player's output path).
    ///
    /// The callback does a downmix into a scratch it reuses and one lock-free
    /// push. It used to take the mutex the interface's meter also takes and
    /// `extend` a `Vec` with no capacity and no ceiling: at a minute of
    /// push-to-talk that buffer is around 11 MB, and the `extend` that
    /// crosses its capacity copies all of it inside the realtime callback —
    /// an audible xrun and a lost stretch of what somebody was saying.
    fn build_input_stream<T: SizedSample, P>(
        device: &Device,
        config: StreamConfig,
        mut producer: P,
    ) -> Result<Stream, RecorderError>
    where
        f32: FromSample<T>,
        P: ringbuf::traits::Producer<Item = f32> + Send + 'static,
    {
        let channels = config.channels as usize;
        // Reused across callbacks; reallocates only past the warmup size.
        let mut mono: Vec<f32> = Vec::with_capacity(2048);
        device
            .build_input_stream(
                config,
                move |data: &[T], _: &cpal::InputCallbackInfo| {
                    mono.clear();
                    if channels == 1 {
                        mono.extend(data.iter().map(|&s| f32::from_sample(s)));
                    } else {
                        for chunk in data.chunks(channels) {
                            let sum: f32 = chunk.iter().map(|&s| f32::from_sample(s)).sum();
                            mono.push(sum / channels as f32);
                        }
                    }
                    // Dropping is the only thing a realtime callback may do
                    // with a full ring, and a full ring means the drain has
                    // been off the CPU for two whole seconds.
                    let _ = producer.push_slice(&mono);
                },
                move |err| {
                    error!("Audio input stream error: {}", err);
                },
                None,
            )
            .map_err(|e| RecorderError::StreamError(e.to_string()))
    }

    #[cfg(test)]
    mod drain_tests {
        use super::*;

        /// The realtime callback used to take the mutex the interface's meter
        /// also takes and `extend` a `Vec` with no capacity and no ceiling,
        /// so a long note reallocated megabytes inside it. Everything that
        /// grows, locks or averages belongs on this side of the ring.
        #[test]
        fn what_the_callback_pushed_becomes_the_capture() {
            let (mut producer, consumer) =
                HeapRb::<f32>::new(ring_samples(CAPTURE_SAMPLE_RATE)).split();
            let capture = Arc::new(Mutex::new(Vec::new()));
            let level = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let (stop, handle) = spawn_drain(
                consumer,
                capture.clone(),
                level.clone(),
                level_window(CAPTURE_SAMPLE_RATE),
                max_recording_samples(CAPTURE_SAMPLE_RATE),
            );

            use ringbuf::traits::Producer as _;
            let block = vec![0.25f32; 4096];
            let mut pushed = 0usize;
            while pushed < 4096 {
                pushed += producer.push_slice(&block[pushed..]);
            }
            drop(producer);
            stop.store(true, Ordering::Relaxed);
            handle.join().expect("the drain finishes");

            assert_eq!(capture.lock().unwrap().len(), 4096);
            assert!(
                f32::from_bits(level.load(Ordering::Relaxed)) > 0.0,
                "the meter is fed by the drain, not by the audio thread"
            );
        }

        /// A device that will not do 48 kHz is opened at its best rate, and
        /// a count of samples is a length of time only against the rate
        /// producing them. Fixed at 48 kHz, a 96 kHz microphone stopped
        /// capturing after five minutes while `stop` went on reporting the
        /// wall clock: a note truncated in silence, claiming a duration
        /// longer than its audio.
        #[test]
        fn the_ceiling_is_a_length_of_time_at_any_rate() {
            assert_eq!(
                max_recording_samples(48_000) / 48_000,
                max_recording_samples(96_000) / 96_000
            );
            assert_eq!(
                max_recording_samples(48_000) / 48_000,
                crate::recorder::MAX_RECORDING_SECS
            );
        }

        /// A capture with nothing stopping it grows until the process is
        /// killed. The ceiling stops it growing; it does not stop the note
        /// that was already recorded from being sent.
        #[test]
        fn a_capture_stops_growing_at_its_ceiling() {
            let ceiling = max_recording_samples(CAPTURE_SAMPLE_RATE);
            let capture = Arc::new(Mutex::new(vec![0.0f32; ceiling - 8]));
            let (mut producer, consumer) =
                HeapRb::<f32>::new(ring_samples(CAPTURE_SAMPLE_RATE)).split();
            let level = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let (stop, handle) = spawn_drain(
                consumer,
                capture.clone(),
                level,
                level_window(CAPTURE_SAMPLE_RATE),
                ceiling,
            );

            use ringbuf::traits::Producer as _;
            let block = vec![0.5f32; 64];
            let mut pushed = 0usize;
            while pushed < block.len() {
                pushed += producer.push_slice(&block[pushed..]);
            }
            drop(producer);
            stop.store(true, Ordering::Relaxed);
            handle.join().expect("the drain finishes");

            assert_eq!(capture.lock().unwrap().len(), ceiling);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{level_window, rms};

        /// The window is 150ms of whatever the device opened at. A count
        /// fixed to 48 kHz is 450ms of tail on a 16 kHz microphone, and the
        /// meter then lags the voice with nothing to show for it.
        #[test]
        fn the_meter_window_follows_the_rate_the_device_opened_at() {
            for rate in [48_000, 44_100, 16_000, 8_000] {
                let window = level_window(rate);
                let ms = window * 1000 / rate as usize;
                assert_eq!(ms, 150, "at {rate} Hz the window is {window} samples");
            }
        }

        #[test]
        fn silence_reads_as_nothing() {
            assert_eq!(rms(&[]), 0.0);
            assert_eq!(rms(&[0.0; 128]), 0.0);
        }

        #[test]
        fn a_speaking_level_lands_in_the_middle_of_the_meter() {
            // ~0.12 RMS is an ordinary speaking voice off a laptop microphone.
            let level = rms(&[0.12; 512]);
            assert!(
                (0.3..0.7).contains(&level),
                "expected a mid-meter reading, got {level}"
            );
        }

        #[test]
        fn a_loud_burst_pins_the_meter_without_passing_it() {
            assert_eq!(rms(&[1.0; 512]), 1.0);
            assert_eq!(rms(&[-1.0; 512]), 1.0);
        }

        use super::*;

        #[test]
        fn resample_interpolates_between_source_samples() {
            // A linear ramp resamples to exact fractional positions; the old
            // nearest-neighbor drop would return [0.0, 1.0, 3.0, 4.0].
            let audio = RecordedAudio {
                samples: vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0],
                sample_rate: 24_000,
                duration_secs: 0,
            };
            assert_eq!(audio.resample_to_16khz(), vec![0.0, 1.5, 3.0, 4.5]);
        }

        #[test]
        fn resample_decimation_has_unity_dc_gain() {
            // The FIR taps are normalized to unity DC gain: a constant signal
            // must come out at the same level (edges included — they clamp to
            // the boundary sample, so even they see pure DC).
            let audio = RecordedAudio {
                samples: vec![0.5; 4800],
                sample_rate: 48_000,
                duration_secs: 0,
            };
            let out = audio.resample_to_16khz();
            assert_eq!(out.len(), 1600);
            for &s in &out {
                assert!((s - 0.5).abs() < 1e-3, "DC gain drifted: {s}");
            }
        }

        #[test]
        fn resample_decimation_attenuates_aliasing_band() {
            // 12kHz at 48k folds to 4kHz after naive 3:1 decimation — squarely
            // in the voice band. The low-pass must crush it while passing 1kHz
            // essentially untouched.
            let rate = 48_000u32;
            let tone = |freq: f32| -> RecordedAudio {
                RecordedAudio {
                    samples: (0..rate as usize)
                        .map(|i| (std::f32::consts::TAU * freq * i as f32 / rate as f32).sin())
                        .collect(),
                    sample_rate: rate,
                    duration_secs: 1,
                }
            };
            let rms = |samples: &[f32]| -> f32 {
                (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
            };
            let low = tone(1_000.0).resample_to_16khz();
            let high = tone(12_000.0).resample_to_16khz();
            // Skip the clamped edges; a full-scale sine has ~0.707 rms.
            let low_rms = rms(&low[100..low.len() - 100]);
            let high_rms = rms(&high[100..high.len() - 100]);
            assert!(low_rms > 0.65, "1kHz should pass through, rms {low_rms}");
            assert!(
                high_rms < 0.02,
                "12kHz should alias-filter to near silence, rms {high_rms}"
            );
        }
    }
}

#[cfg(not(target_family = "wasm"))]
pub use capture::AudioRecorder;

#[derive(Debug)]
pub enum RecorderError {
    NoInputDevice,
    NoSupportedConfig,
    NotInitialized,
    AlreadyRecording,
    NotRecording,
    DeviceError(String),
    StreamError(String),
}

impl std::fmt::Display for RecorderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoInputDevice => write!(f, "No audio input device found"),
            Self::NoSupportedConfig => write!(f, "No supported audio configuration found"),
            Self::NotInitialized => write!(f, "Recorder not initialized"),
            Self::AlreadyRecording => write!(f, "Already recording"),
            Self::NotRecording => write!(f, "Not recording"),
            Self::DeviceError(e) => write!(f, "Audio device error: {}", e),
            Self::StreamError(e) => write!(f, "Audio stream error: {}", e),
        }
    }
}

impl std::error::Error for RecorderError {}
