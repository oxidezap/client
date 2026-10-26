//! Inventory never opens a stream. Ringtone owns only an output stream.
use anyhow::Result;
use oxidezap_core::{CallDevice, Ringtone};

pub use imp::{RingtonePlayback, enumerate_audio_devices, start_ringtone};

/// A deterministic, quiet two-tone signal with a silent interval.
pub fn ringtone_sample(tone: Ringtone, frame: u64, rate: u32) -> i16 {
    if tone == Ringtone::Silent || rate == 0 || frame % (u64::from(rate) * 3) >= u64::from(rate) {
        return 0;
    }
    let hz = if tone == Ringtone::Gentle {
        440.0
    } else {
        660.0
    };
    let phase = frame as f64 / f64::from(rate);
    let envelope = (phase % 3.0 * 40.0).min(1.0);
    ((phase * hz * std::f64::consts::TAU).sin() * 2000.0 * envelope) as i16
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use super::*;
    use anyhow::anyhow;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use portable_atomic::AtomicU64;
    use ringbuf::traits::{Producer, Split};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    /// Dropping this guard cancels startup as well as playback.
    pub struct RingtonePlayback(Arc<AtomicBool>);
    impl Drop for RingtonePlayback {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    pub fn enumerate_audio_devices() -> Result<(Vec<CallDevice>, Vec<CallDevice>)> {
        let host = cpal::default_host();
        let input_default = host.default_input_device().and_then(|d| d.id().ok());
        let output_default = host.default_output_device().and_then(|d| d.id().ok());
        fn entries(
            devices: impl Iterator<Item = cpal::Device>,
            default: Option<cpal::DeviceId>,
        ) -> Vec<CallDevice> {
            devices
                .filter_map(|d| {
                    let id = d.id().ok()?;
                    Some(CallDevice {
                        is_default: default.as_ref() == Some(&id),
                        id: id.to_string(),
                        label: d.description().ok()?.name().to_owned(),
                        modes: Vec::new(),
                    })
                })
                .collect()
        }
        Ok((
            entries(host.input_devices()?, input_default),
            entries(host.output_devices()?, output_default),
        ))
    }

    pub(crate) fn select(id: Option<&str>, input: bool) -> Result<cpal::Device> {
        let host = cpal::default_host();
        match id {
            Some(id) => {
                let id = id
                    .parse::<cpal::DeviceId>()
                    .map_err(|_| anyhow!("invalid audio device id"))?;
                host.device_by_id(&id).ok_or_else(|| anyhow!("selected audio device is unavailable; refresh devices or choose System default"))
            }
            None => if input {
                host.default_input_device()
            } else {
                host.default_output_device()
            }
            .ok_or_else(|| anyhow!("no system default audio device")),
        }
    }

    pub fn start_ringtone(output: Option<String>, tone: Ringtone) -> Result<RingtonePlayback> {
        let stop = Arc::new(AtomicBool::new(false));
        let guard = RingtonePlayback(stop.clone());
        if tone == Ringtone::Silent {
            return Ok(guard);
        }
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let opened = (|| -> Result<_> {
                let device = select(output.as_deref(), false)?;
                let (config, format) = crate::call_device::default_config(&device, false)?;
                let (producer, consumer) =
                    ringbuf::HeapRb::<i16>::new(config.sample_rate as usize).split();
                let on_error = {
                    let stop = stop.clone();
                    Arc::new(move || stop.store(true, Ordering::Release))
                };
                let stream = crate::call_device::build_output_stream(
                    &device,
                    &config,
                    format,
                    config.channels as usize,
                    consumer,
                    crate::call_device::SpeakerCounters {
                        pops: Arc::new(AtomicU64::new(0)),
                        underruns: Arc::new(AtomicU64::new(0)),
                    },
                    on_error,
                )?;
                stream.play()?;
                Ok((stream, config.sample_rate, producer))
            })();
            let Ok((_stream, rate, mut producer)) = opened else {
                let _ = ready_tx.send(Err(anyhow!("ringtone output could not start")));
                return;
            };
            let _ = ready_tx.send(Ok(()));
            let mut frame = 0;
            while !stop.load(Ordering::Acquire) {
                for _ in 0..(rate / 100) {
                    if producer
                        .try_push(ringtone_sample(tone, frame, rate))
                        .is_err()
                    {
                        break;
                    }
                    frame += 1;
                }
                std::thread::park_timeout(Duration::from_millis(5));
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("ringtone output startup timed out"))??;
        Ok(guard)
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use super::*;
    pub struct RingtonePlayback;
    pub fn enumerate_audio_devices() -> Result<(Vec<CallDevice>, Vec<CallDevice>)> {
        anyhow::bail!("Device inventory is unavailable from this browser connection")
    }
    pub fn start_ringtone(_output: Option<String>, tone: Ringtone) -> Result<RingtonePlayback> {
        if tone == Ringtone::Silent {
            Ok(RingtonePlayback)
        } else {
            anyhow::bail!("Ringtone playback is unavailable from this browser connection")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tones_are_bounded_and_have_silent_gaps() {
        for tone in [Ringtone::Classic, Ringtone::Gentle, Ringtone::Silent] {
            assert!((0..16000).all(|i| ringtone_sample(tone, i, 16000).unsigned_abs() <= 2000));
            assert!((16000..48000).all(|i| ringtone_sample(tone, i, 16000) == 0));
        }
        assert!((0..16000).all(|i| ringtone_sample(Ringtone::Silent, i, 16000) == 0));
    }
}
