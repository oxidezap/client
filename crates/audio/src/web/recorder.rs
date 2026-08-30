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
    /// Which recording the capture state belongs to.
    ///
    /// `getUserMedia` prompts, so a person who starts and immediately cancels
    /// leaves an open still in flight. Without this it resolved into the
    /// state anyway and the microphone stayed live — indicator on, samples
    /// accumulating — until another recording replaced it. The opener carries
    /// the generation it started under and closes what it opened if that is
    /// no longer the current one.
    generation: u64,
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
        stop_tracks(&self.stream);
        let _ = self.context.close();
    }
}

/// Stops the tracks it holds unless the recording took them.
///
/// Between `getUserMedia` answering and the graph being wired there are four
/// fallible steps, and on every one of them the microphone is already live.
struct Tracks(web_sys::MediaStream);

impl Tracks {
    /// Hand the stream over to something that will close it.
    fn disarm(self) -> web_sys::MediaStream {
        let stream = self.0.clone();
        std::mem::forget(self);
        stream
    }
}

impl Drop for Tracks {
    fn drop(&mut self) {
        stop_tracks(&self.0);
    }
}

/// The same for the context, which holds the hardware open in its own right.
struct Closing(web_sys::AudioContext);

impl Closing {
    fn disarm(self) -> web_sys::AudioContext {
        let context = self.0.clone();
        std::mem::forget(self);
        context
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        let _ = self.0.close();
    }
}

/// Stop every track of a capture stream.
fn stop_tracks(stream: &web_sys::MediaStream) {
    for track in stream.get_tracks().iter() {
        if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
            track.stop();
        }
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
        if !js_sys::Reflect::has(&window, &"AudioEncoder".into()).unwrap_or(false)
            || window.navigator().media_devices().is_err()
        {
            return false;
        }
        // Having an `AudioEncoder` is not having Opus, so the configuration
        // this recorder will actually use is tried here. What that catches is
        // the synchronous half: a configuration the browser rejects outright.
        //
        // The other half is asynchronous — an unsupported *codec* is reported
        // on the error callback rather than thrown — and the API that asks
        // properly, `isConfigSupported`, is behind `web_sys_unstable_apis`,
        // which is a build flag and not something to turn on as a side effect
        // of this. So a browser that accepts the shape and then refuses Opus
        // still reaches the failure at the end of a recording, where it is at
        // least *said*: the error travels back through `Recording::Pending`
        // and onto the notice surface rather than into a log.
        let init = web_sys::AudioEncoderInit::new(
            &js_sys::Function::new_no_args(""),
            &js_sys::Function::new_no_args(""),
        );
        let Ok(probe) = web_sys::AudioEncoder::new(&init) else {
            return false;
        };
        let config = web_sys::AudioEncoderConfig::new("opus", 1, TARGET_SAMPLE_RATE);
        config.set_bitrate(BITRATE);
        let accepted = probe.configure(&config).is_ok();
        let _ = probe.close();
        accepted
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
        let generation = {
            let mut capture = self.capture.borrow_mut();
            let next = capture.generation.wrapping_add(1);
            *capture = Capture {
                generation: next,
                ..Capture::default()
            };
            next
        };
        self.recording = true;

        // Built here, in the gesture, and not after the permission prompt.
        // A browser grants an audio context permission to run only under a
        // transient user activation, and `getUserMedia` is a dialog somebody
        // takes seconds to answer — so a context constructed on the far side
        // of that await can be born suspended. What that looks like is not an
        // error: the permission is granted, the tracks are live, the node is
        // connected, and `onaudioprocess` simply never fires, so stopping
        // reports a microphone that produced nothing.
        //
        // The same reason the video path calls `unlock` before it downloads.
        let context = web_sys::AudioContext::new().map_err(|e| {
            self.recording = false;
            RecorderError::DeviceError(format!("no audio context to record with: {e:?}"))
        })?;
        // Suspended is the state this is here to leave, and the answer is a
        // promise nothing waits on: by the time it resolves the microphone is
        // still opening, and a context that was already running ignores it.
        let _ = context.resume();
        let context = Closing(context);

        let capture = Rc::clone(&self.capture);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = open_microphone(&capture, generation, context).await {
                let mut capture = capture.borrow_mut();
                if capture.generation == generation {
                    capture.failed = Some(e);
                }
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
            // indicator should go out when the person stops talking. Moving
            // the generation on is what closes an open still in flight.
            capture.teardown = None;
            capture.generation = capture.generation.wrapping_add(1);
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
        capture.generation = capture.generation.wrapping_add(1);
        capture.samples.clear();
        capture.level = 0.0;
    }
}

/// Open the device and wire the capture graph.
async fn open_microphone(
    capture: &Rc<RefCell<Capture>>,
    generation: u64,
    context: Closing,
) -> Result<(), String> {
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

    // Owned from here on. Every `?` below is a path where the permission has
    // already been granted and the tracks are already live, so a failure that
    // merely dropped the JS wrapper would leave the microphone open with its
    // indicator on and nothing holding it.
    let held = Tracks(stream);

    // The context came from `start`, so it was built while the gesture was
    // still live. It is still a guard, and every `?` below still closes it.
    let source = context
        .0
        .create_media_stream_source(&held.0)
        .map_err(|e| format!("the microphone could not be attached: {e:?}"))?;
    let node = context
        .0
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
                if capture.generation != generation {
                    return;
                }
                capture.level = rms(&channel);
                // The same ten-minute ceiling the desktop capture holds, and
                // it matters more here: a tab's linear memory has a fixed
                // roof, and an allocation that fails past it aborts rather
                // than returning an error. A recording left running would
                // otherwise take the whole application down.
                // The buffer's own rate rather than the capture's: this
                // callback can run before the open that records it, and a
                // ceiling derived from zero is a recording six hundred
                // samples long.
                let ceiling =
                    crate::recorder::max_recording_samples(buffer.sample_rate().max(1.0) as u32);
                if capture.samples.len() >= ceiling {
                    return;
                }
                let room = ceiling - capture.samples.len();
                let taking = channel.len().min(room);
                capture.samples.extend_from_slice(&channel[..taking]);
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
    node.connect_with_audio_node(&context.0.destination())
        .map_err(|e| format!("the capture graph would not run: {e:?}"))?;

    let mut capture = capture.borrow_mut();
    // Stopped or cancelled while the prompt was up: the guards below go out of
    // scope with everything they hold, which is what closes the device.
    if capture.generation != generation {
        return Ok(());
    }
    capture.sample_rate = context.0.sample_rate() as u32;
    capture.teardown = Some(Teardown {
        context: context.disarm(),
        stream: held.disarm(),
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
    // The same rule the desktop encoder follows, and the reason the check is
    // shared: where the final frame's zero-padding cannot absorb the
    // pre-skip — and an exact 20ms multiple has no padding at all — the
    // packets stop short of the granule the header promises, and a decoder
    // trims into real audio. One more encoded frame of silence covers it.
    let packets = if crate::ogg_opus::needs_trailing_silence(packets.len(), at_target.len()) {
        let mut padded = packets;
        padded.extend(encode_opus(&vec![0.0; crate::ogg_opus::FRAME_SIZE_SAMPLES]).await?);
        padded
    } else {
        packets
    };
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
        // Refused rather than skipped. Packaging counts the *samples* that
        // were captured, so a frame quietly left out makes the stream stop
        // short of the granule the header promises: a note reported as
        // encoded and truncated when it is played.
        let Ok(audio) = web_sys::AudioData::new(&init) else {
            // Closed before returning: the browser holds references to the
            // two closures below, and dropping them while it can still call
            // one is a call into freed memory, which takes the tab.
            let _ = encoder.close();
            return Err(RecorderError::DeviceError(
                "the browser would not take a frame of the recording".to_string(),
            ));
        };
        let encoded = encoder.encode(&audio);
        audio.close();
        if let Err(e) = encoded {
            let _ = encoder.close();
            return Err(RecorderError::DeviceError(format!(
                "the browser refused a frame of the recording: {e:?}"
            )));
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

/// What the desktop meter multiplies by, and why this one has to as well.
///
/// Both values reach the same `render_level`, so a bare RMS here would draw a
/// quarter of the bar for the same voice: an ordinary speaking voice is about
/// 0.12 RMS, which is a decoration rather than a meter.
const LEVEL_GAIN: f32 = 4.0;

/// Root mean square of a block, scaled the way the meter is drawn.
fn rms(block: &[f32]) -> f32 {
    if block.is_empty() {
        return 0.0;
    }
    let sum: f32 = block.iter().map(|s| s * s).sum();
    ((sum / block.len() as f32).sqrt() * LEVEL_GAIN).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Silence reads as no level, and the gain is the desktop's: both values
    /// reach the same meter, so this one has to be scaled the same way.
    #[test]
    fn the_level_is_the_gained_root_mean_square() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0.0, 0.0, 0.0]), 0.0);
        // Full scale is already past the top of the meter, so it clamps.
        assert!((rms(&[1.0, -1.0, 1.0, -1.0]) - 1.0).abs() < f32::EPSILON);
        // An ordinary speaking voice: about half the bar, not an eighth.
        assert!((rms(&[0.12, -0.12]) - 0.48).abs() < 1e-5);
    }
}
