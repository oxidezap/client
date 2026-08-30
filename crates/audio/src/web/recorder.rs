//! Recording a voice note in a browser.
//!
//! The desktop captures with cpal and encodes with libopus. Only the second
//! of those was ever missing here: libopus is C, and `wasm32-unknown-unknown`
//! has no C toolchain. The browser has an Opus encoder of its own and hands
//! it over as WebCodecs, so this captures through WebAudio and encodes
//! through [`web_sys::AudioEncoder`].
//!
//! # Why the container is not the problem
//!
//! `MediaRecorder` is the obvious route and the wrong one. It produces a
//! *container* the browser picks — WebM on Chrome, MP4 on Safari — and a
//! WhatsApp voice note is Opus in OGG. Taking it would have meant either
//! shipping a note most recipients cannot play, or writing a WebM demuxer to
//! get the packets back out.
//!
//! `AudioEncoder` hands back the Opus packets directly, with no container at
//! all, and [`crate::ogg_opus`] wraps them in the same stream the desktop
//! writes. So the bytes a recipient receives come from one packager on both
//! platforms rather than from two that would have to agree.
//!
//! # Where it is asynchronous, and what that costs
//!
//! Three things here happen later than the call that asks for them: the
//! microphone (a permission prompt), the encoder's packets, and the flush at
//! the end. `start` is therefore optimistic — it opens the device on a task
//! and reports a refusal through [`AudioRecorder::stop`], which is where the
//! caller already handles failure — and `stop` answers a channel rather than
//! a value. [`crate::Recording`] is that difference, named once.
//!
//! # ScriptProcessorNode
//!
//! Deprecated, and used deliberately. The replacement is `AudioWorklet`,
//! which is loaded from a JavaScript module file; the one hand-written JS
//! file in this tree is the service worker, and it exists because nothing
//! else can set a response header. A deprecated node that works everywhere
//! beats a second JavaScript file.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

use crate::recorder::{EncodedNote, RecordedAudio, RecorderError, Recording, TARGET_SAMPLE_RATE};

/// How many frames the capture node hands over at a time.
///
/// A power of two between 256 and 16384, per the WebAudio specification. At
/// the low end the main thread is woken constantly; at the high end the level
/// meter lags visibly behind the voice. 4096 at 48 kHz is about 85ms.
const CAPTURE_CHUNK: u32 = 4096;

/// What a voice note is encoded at, and what WhatsApp's own are.
const BITRATE: u32 = 16_000;

/// Everything one recording holds, shared with the callbacks driving it.
#[derive(Default)]
struct Capture {
    /// Every sample captured, at whatever rate the context runs.
    samples: Vec<f32>,
    /// The context's own rate, learned when it is opened.
    sample_rate: u32,
    /// The newest level, for the meter the composer draws.
    level: f32,
    /// Set when the microphone was refused or the encoder stopped, and
    /// answered at [`AudioRecorder::stop`] because that is where the caller
    /// is already prepared to hear it.
    failed: Option<String>,
    /// Dropped when the recording ends, which is what closes the device: a
    /// track left live is a browser tab with its microphone indicator on.
    teardown: Option<Teardown>,
}

/// The objects a live recording keeps alive, released together.
struct Teardown {
    context: web_sys::AudioContext,
    stream: web_sys::MediaStream,
    /// Kept because the browser calls into it: a `Closure` dropped while it
    /// is still referenced is a call into freed memory, which takes the tab.
    _on_audio: Closure<dyn FnMut(web_sys::AudioProcessingEvent)>,
    _node: web_sys::ScriptProcessorNode,
    _source: web_sys::MediaStreamAudioSourceNode,
}

impl Drop for Teardown {
    fn drop(&mut self) {
        for track in self.stream.get_tracks().iter() {
            if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                track.stop();
            }
        }
        let _ = self.context.close();
    }
}

/// The browser's microphone, behind the desktop recorder's API.
#[derive(Default)]
pub struct AudioRecorder {
    capture: Rc<RefCell<Capture>>,
    recording: bool,
}

impl AudioRecorder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this browser has the two APIs a recording needs.
    ///
    /// Asked before the microphone is offered rather than after it is opened,
    /// which is what `CAN_RECORD` was for on the desktop: a control that is
    /// drawn and then always fails is worse than one that is not drawn. It is
    /// a question about the *runtime* here rather than about the build, which
    /// is why it is a function.
    #[must_use]
    pub fn supported() -> bool {
        let Some(window) = web_sys::window() else {
            return false;
        };
        let has_encoder = js_sys::Reflect::has(&window, &"AudioEncoder".into()).unwrap_or(false);
        let has_devices = window.navigator().media_devices().is_ok();
        has_encoder && has_devices
    }

    /// # Errors
    ///
    /// This browser has no `AudioEncoder` or no microphone API.
    pub fn init(&mut self) -> Result<(), RecorderError> {
        if Self::supported() {
            Ok(())
        } else {
            Err(RecorderError::DeviceError(
                "this browser has no Opus encoder for voice notes".to_string(),
            ))
        }
    }

    /// Open the microphone and begin capturing.
    ///
    /// Optimistic: `getUserMedia` prompts, so the device is opened on a task
    /// and a refusal is reported at [`Self::stop`]. Answering `Ok` for a
    /// microphone that turns out to be denied is the same shape the desktop
    /// has for a device that disappears mid-recording.
    ///
    /// # Errors
    ///
    /// Already recording, or the browser has no microphone API at all.
    pub fn start(&mut self) -> Result<(), RecorderError> {
        if self.recording {
            return Err(RecorderError::AlreadyRecording);
        }
        self.init()?;
        *self.capture.borrow_mut() = Capture::default();
        self.recording = true;

        let capture = Rc::clone(&self.capture);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = open_microphone(&capture).await {
                capture.borrow_mut().failed = Some(e);
            }
        });
        Ok(())
    }

    /// The newest input level, for the meter.
    #[must_use]
    pub fn level(&self) -> f32 {
        self.capture.borrow().level
    }

    /// Close the microphone and encode what was captured.
    ///
    /// # Errors
    ///
    /// Not recording. Everything else — a refused microphone, an encoder that
    /// stopped, a recording with nothing in it — arrives through the channel,
    /// because it is not known yet.
    pub fn stop(&mut self) -> Result<Recording, RecorderError> {
        if !self.recording {
            return Err(RecorderError::NotRecording);
        }
        self.recording = false;

        let (samples, sample_rate, failed) = {
            let mut capture = self.capture.borrow_mut();
            // Dropping the teardown is what closes the device, and it happens
            // here rather than when the encode finishes: the microphone
            // indicator should go out when the person stops talking.
            capture.teardown = None;
            (
                std::mem::take(&mut capture.samples),
                capture.sample_rate,
                capture.failed.take(),
            )
        };

        let (tx, rx) = futures_channel::oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            let outcome = match failed {
                Some(e) => Err(RecorderError::DeviceError(e)),
                None => encode(samples, sample_rate).await,
            };
            // Nobody listening is a recording that was cancelled while it
            // encoded, which is not a failure worth reporting anywhere.
            let _ = tx.send(outcome);
        });
        Ok(Recording::Pending(rx))
    }

    /// Drop the recording and close the device.
    pub fn cancel(&mut self) {
        self.recording = false;
        let mut capture = self.capture.borrow_mut();
        capture.teardown = None;
        capture.samples.clear();
        capture.level = 0.0;
    }
}

/// Open the device and wire the capture graph.
async fn open_microphone(capture: &Rc<RefCell<Capture>>) -> Result<(), String> {
    let window = web_sys::window().ok_or("no window to record from")?;
    let devices = window
        .navigator()
        .media_devices()
        .map_err(|e| format!("this browser offers no microphone: {e:?}"))?;

    let constraints = web_sys::MediaStreamConstraints::new();
    constraints.set_audio(&wasm_bindgen::JsValue::TRUE);
    let stream = wasm_bindgen_futures::JsFuture::from(
        devices
            .get_user_media_with_constraints(&constraints)
            .map_err(|e| format!("the microphone could not be opened: {e:?}"))?,
    )
    .await
    .map_err(|_| "the microphone was refused".to_string())?
    .dyn_into::<web_sys::MediaStream>()
    .map_err(|_| "the browser opened something that is not a stream".to_string())?;

    let context = web_sys::AudioContext::new()
        .map_err(|e| format!("no audio context to record with: {e:?}"))?;
    let source = context
        .create_media_stream_source(&stream)
        .map_err(|e| format!("the microphone could not be attached: {e:?}"))?;
    let node = context
        .create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(
            CAPTURE_CHUNK,
            1,
            1,
        )
        .map_err(|e| format!("no capture node: {e:?}"))?;

    let on_audio = {
        let capture = Rc::clone(capture);
        Closure::<dyn FnMut(web_sys::AudioProcessingEvent)>::new(
            move |event: web_sys::AudioProcessingEvent| {
                let Ok(buffer) = event.input_buffer() else {
                    return;
                };
                let Ok(channel) = buffer.get_channel_data(0) else {
                    return;
                };
                let mut capture = capture.borrow_mut();
                capture.level = rms(&channel);
                capture.samples.extend_from_slice(&channel);
            },
        )
    };
    node.set_onaudioprocess(Some(on_audio.as_ref().unchecked_ref()));

    source
        .connect_with_audio_node(&node)
        .map_err(|e| format!("the capture graph would not connect: {e:?}"))?;
    // A ScriptProcessorNode does not run unless it reaches the destination,
    // even though nothing here wants to hear the input: the output buffer is
    // left untouched and therefore silent, so this feeds the speakers zeros
    // rather than the microphone.
    node.connect_with_audio_node(&context.destination())
        .map_err(|e| format!("the capture graph would not run: {e:?}"))?;

    let mut held = capture.borrow_mut();
    held.sample_rate = context.sample_rate() as u32;
    held.teardown = Some(Teardown {
        context,
        stream,
        _on_audio: on_audio,
        _node: node,
        _source: source,
    });
    Ok(())
}

/// Encode captured samples into the voice note that gets sent.
async fn encode(samples: Vec<f32>, sample_rate: u32) -> Result<EncodedNote, RecorderError> {
    if samples.is_empty() {
        return Err(RecorderError::DeviceError(
            "the microphone produced nothing".to_string(),
        ));
    }
    let duration_secs = (samples.len() as f64 / f64::from(sample_rate.max(1))) as u32;
    let captured = RecordedAudio {
        samples,
        sample_rate,
        duration_secs,
    };
    // The same resampler the desktop uses, and the same waveform: the
    // envelope is measured while these are still samples, because once the
    // browser has handed back packets there is nothing left to measure.
    let at_target = captured.resample_to_16khz();
    let waveform = crate::waveform::generate_waveform(&at_target);

    let packets = encode_opus(&at_target).await?;
    let bytes = crate::ogg_opus::package(packets, at_target.len()).map_err(|e| {
        RecorderError::DeviceError(format!("the voice note would not package: {e}"))
    })?;

    Ok(EncodedNote {
        bytes,
        waveform,
        duration_secs: captured.duration_secs,
    })
}

/// Run the samples through the browser's Opus encoder, in 20ms frames.
async fn encode_opus(samples: &[f32]) -> Result<Vec<Vec<u8>>, RecorderError> {
    use crate::ogg_opus::FRAME_SIZE_SAMPLES;

    let packets: Rc<RefCell<Vec<Vec<u8>>>> = Rc::new(RefCell::new(Vec::new()));
    let failure: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let on_output = {
        let packets = Rc::clone(&packets);
        Closure::<dyn FnMut(web_sys::EncodedAudioChunk)>::new(
            move |chunk: web_sys::EncodedAudioChunk| {
                let size = chunk.byte_length() as usize;
                let mut packet = vec![0u8; size];
                if chunk.copy_to_with_u8_slice(&mut packet).is_ok() {
                    packets.borrow_mut().push(packet);
                }
            },
        )
    };
    let on_error = {
        let failure = Rc::clone(&failure);
        Closure::<dyn FnMut(web_sys::DomException)>::new(move |e: web_sys::DomException| {
            let mut failure = failure.borrow_mut();
            if failure.is_none() {
                *failure = Some(e.message());
            }
        })
    };

    let init = web_sys::AudioEncoderInit::new(
        on_error.as_ref().unchecked_ref(),
        on_output.as_ref().unchecked_ref(),
    );
    let encoder = web_sys::AudioEncoder::new(&init)
        .map_err(|e| RecorderError::DeviceError(format!("no Opus encoder: {e:?}")))?;

    let config = web_sys::AudioEncoderConfig::new("opus", 1, TARGET_SAMPLE_RATE);
    config.set_bitrate(BITRATE);
    encoder
        .configure(&config)
        .map_err(|e| RecorderError::DeviceError(format!("the encoder refused Opus: {e:?}")))?;

    // Padded to a whole frame, exactly as the desktop pads its last chunk:
    // the packet stream has to cover the granule the header promises.
    // Microseconds, and an `i32` because that is what the binding takes: a
    // voice note runs out at about half an hour, which is far past anything
    // WhatsApp accepts as one.
    let mut micros: i32 = 0;
    let frame_micros = (1_000_000 * FRAME_SIZE_SAMPLES as i32) / TARGET_SAMPLE_RATE as i32;
    for chunk in samples.chunks(FRAME_SIZE_SAMPLES) {
        let mut frame = chunk.to_vec();
        frame.resize(FRAME_SIZE_SAMPLES, 0.0);
        let data = js_sys::Float32Array::from(frame.as_slice());
        let init = web_sys::AudioDataInit::new(
            &data,
            web_sys::AudioSampleFormat::F32,
            1,
            FRAME_SIZE_SAMPLES as u32,
            TARGET_SAMPLE_RATE as f32,
            micros,
        );
        let Ok(audio) = web_sys::AudioData::new(&init) else {
            continue;
        };
        let encoded = encoder.encode(&audio);
        audio.close();
        if encoded.is_err() {
            break;
        }
        micros = micros.saturating_add(frame_micros);
    }

    // Awaited, because the packets are what this function is for: an encoder
    // dropped without flushing answers with whatever happened to be out.
    let flushed = wasm_bindgen_futures::JsFuture::from(encoder.flush()).await;
    let _ = encoder.close();
    if let Some(e) = failure.borrow().clone() {
        return Err(RecorderError::DeviceError(format!(
            "the browser's encoder stopped: {e}"
        )));
    }
    flushed.map_err(|e| RecorderError::DeviceError(format!("the encode did not finish: {e:?}")))?;

    let packets = std::mem::take(&mut *packets.borrow_mut());
    if packets.is_empty() {
        return Err(RecorderError::DeviceError(
            "the encoder produced nothing".to_string(),
        ));
    }
    Ok(packets)
}

/// Root mean square of a block, which is what the meter draws.
fn rms(block: &[f32]) -> f32 {
    if block.is_empty() {
        return 0.0;
    }
    let sum: f32 = block.iter().map(|s| s * s).sum();
    (sum / block.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Silence reads as no level, and a full-scale block as one: the meter is
    /// drawn straight from this.
    #[test]
    fn the_level_is_the_root_mean_square() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0.0, 0.0, 0.0]), 0.0);
        assert!((rms(&[1.0, -1.0, 1.0, -1.0]) - 1.0).abs() < f32::EPSILON);
        assert!((rms(&[0.5, -0.5]) - 0.5).abs() < f32::EPSILON);
    }
}
