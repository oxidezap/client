//! A call's microphone and speaker, in a browser.
//!
//! The desktop opens both through cpal and hands the engine 60 ms of 16 kHz
//! mono `i16`. A page reaches the same two devices through WebAudio, and this
//! is the same bridge over them: capture, downsample, chunk, send; receive,
//! upsample, play.
//!
//! Nothing about it is a stub any more. It used to be — a page held no
//! session, so it opened no device — and the sentence that justified that is
//! still true of a page attached to an `oxidezapd`. What changed is that a
//! page can hold the session itself, and the process that owns the session
//! owns the microphone whichever process that is.
//!
//! # One context for both directions
//!
//! Deliberate, and the reason is echo. The browser's own acoustic echo
//! canceller subtracts what it is *playing* from what it captures, which it
//! can only do for audio it played itself — so the peer's voice has to leave
//! through the same graph the microphone is attached to, or the caller hears
//! themselves back a beat late. That is also why the capture constraints ask
//! for `echoCancellation`, `noiseSuppression` and `autoGainControl` rather
//! than taking the raw device: cpal gets whatever the OS mixer already
//! applied, and a browser applies none of it unless asked.
//!
//! # ScriptProcessorNode
//!
//! Deprecated and used deliberately, for the reason [`super::recorder`] gives
//! at length: the replacement is an `AudioWorklet`, which is loaded from a
//! JavaScript module file, and the one hand-written JS file in this tree is
//! the service worker. It is used on both sides here — the capture node
//! leaves its output buffer untouched, which is silence, and the playout node
//! declares no input at all.
//!
//! # Where a frame is dropped
//!
//! Both callbacks run on the page's audio thread and neither may wait. The
//! microphone's send is a `try_send` onto a short queue, because a frame the
//! engine has not taken by the time the next one is captured is a frame worth
//! less than the delay of holding it; the speaker's ring is bounded for the
//! same reason and drops from the *front*, since the oldest audio in a call
//! is the audio nobody wants.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use anyhow::{Result, anyhow, bail};
use log::{debug, warn};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

use crate::{CALL_FRAME_SAMPLES, CALL_RATE};

/// How many samples a WebAudio callback carries at a time.
///
/// A power of two between 256 and 16384, per the specification. 1024 at
/// 48 kHz is about 21 ms — a third of the 60 ms frame the engine wants, so
/// three callbacks fill one frame and the added latency is a callback rather
/// than a frame. The recorder uses 4096 because a voice note is not a
/// conversation and a level meter that lags is the only cost there.
const CHUNK: u32 = 1024;

/// How many captured frames may wait for the engine.
///
/// Short on purpose: this is live audio, and a queue is latency rather than
/// safety. Four frames is a quarter of a second, which is already more delay
/// than a call should carry.
const MIC_DEPTH: usize = 4;

/// How many decoded frames may wait for the speaker, in samples at the
/// context's own rate.
///
/// About half a second at 48 kHz. Enough to ride out the jitter of a network
/// and of the page's own scheduler; short enough that a stall does not turn
/// into audio arriving from the past.
const PLAYOUT_CEILING: usize = 24_000;

/// How much silence the ring is primed with before playout starts.
///
/// The callback runs on a strict clock and the network does not, so a ring
/// that starts empty emits a gap on its very first callback and then again on
/// every packet that is a moment late. 60 ms is one frame of headroom, paid
/// once.
const PLAYOUT_PRIME: usize = 60;

/// What one call's audio graph keeps alive, released together.
///
/// The closures are held because the browser calls into them: a `Closure`
/// dropped while it is still referenced is a call into freed memory, which
/// takes the tab. Dropping this closes the microphone — the track is stopped
/// explicitly, because a `MediaStream` merely dropped leaves the tab's
/// recording indicator on — and then the context.
struct Graph {
    context: web_sys::AudioContext,
    stream: web_sys::MediaStream,
    capture: web_sys::ScriptProcessorNode,
    playout: web_sys::ScriptProcessorNode,
    _on_capture: Closure<dyn FnMut(web_sys::AudioProcessingEvent)>,
    _on_playout: Closure<dyn FnMut(web_sys::AudioProcessingEvent)>,
}

impl Drop for Graph {
    fn drop(&mut self) {
        // Detached first: a node still wired to the destination goes on being
        // called while the context closes, and `close` is asynchronous.
        self.capture.set_onaudioprocess(None);
        self.playout.set_onaudioprocess(None);
        let _ = self.capture.disconnect();
        let _ = self.playout.disconnect();
        for track in self.stream.get_tracks().iter() {
            if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                track.stop();
            }
        }
        let _ = self.context.close();
        debug!("the call's audio graph is closed");
    }
}

/// Releases the context and the microphone if setup does not reach [`Graph`].
///
/// [`Graph`] closes both when it drops, which covers every ending of a call
/// that started. What it does not cover is a setup that fails *before* it
/// exists: `wire` can refuse at any of half a dozen browser calls, and a
/// `MediaStream` that is only dropped keeps the microphone — and its
/// indicator — running, because the specification ends a source on `stop()`
/// and not on the last reference going away.
struct Opening {
    context: web_sys::AudioContext,
    stream: Option<web_sys::MediaStream>,
}

impl Opening {
    /// Setup finished; [`Graph`] closes them from here.
    fn release(mut self) -> web_sys::MediaStream {
        self.stream.take().expect("a guard is released once")
    }
}

impl Drop for Opening {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            for track in stream.get_tracks().iter() {
                if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                    track.stop();
                }
            }
            let _ = self.context.close();
            debug!("the call's audio is closed again: it opened but its setup did not finish");
        }
    }
}

/// Open the microphone and the speaker for one call.
///
/// Async where the desktop's is blocking, and it has to be: `getUserMedia` is
/// a permission prompt. Answered before the offer or the accept goes out, for
/// the reason the desktop opens its devices there too — a call that claims
/// audio it cannot produce is worse than one that was never placed.
pub async fn open_call_audio() -> Result<(
    async_channel::Receiver<Vec<i16>>,
    async_channel::Sender<Vec<i16>>,
)> {
    let context = web_sys::AudioContext::new()
        .map_err(|e| anyhow!("this browser has no AudioContext: {}", describe(&e)))?;
    let rate = context.sample_rate() as u32;

    let opening = Opening {
        context: context.clone(),
        stream: Some(open_microphone().await.inspect_err(|_| {
            let _ = context.close();
        })?),
    };
    let stream = opening.stream.as_ref().expect("just armed").clone();

    let (mic_tx, mic_rx) = async_channel::bounded::<Vec<i16>>(MIC_DEPTH);
    let (speaker_tx, speaker_rx) = async_channel::bounded::<Vec<i16>>(MIC_DEPTH * 4);
    // Primed with one frame of silence; see `PLAYOUT_PRIME`.
    let playout: Rc<RefCell<VecDeque<f32>>> = Rc::new(RefCell::new(VecDeque::from(vec![
        0.0;
        rate as usize * PLAYOUT_PRIME
            / 1000
    ])));

    let graph = wire(&context, &stream, rate, mic_tx.clone(), Rc::clone(&playout))?;

    // Awaited, and then checked. A context opens suspended when the page has
    // had no gesture yet, and `resume` is a promise that autoplay policy may
    // *reject* — a call answered from a notification rather than from a press
    // is exactly that case. Neither `ScriptProcessorNode` is called while the
    // context is suspended, so letting this go unwatched returns a microphone
    // and a speaker that look live, lets the call be offered or accepted, and
    // produces silence in both directions with nothing anywhere saying why.
    let resuming = context
        .resume()
        .map_err(|e| anyhow!("the browser would not start audio: {}", describe(&e)))?;
    if let Err(e) = wasm_bindgen_futures::JsFuture::from(resuming).await {
        bail!(
            "the browser would not start audio for this call: {}",
            describe(&e)
        );
    }
    if context.state() != web_sys::AudioContextState::Running {
        bail!("the browser left this call's audio suspended, so nothing would be heard");
    }

    // Past every fallible step: `Graph` owns the context and the microphone
    // from here, and closes both when the call ends.
    let _stream = opening.release();

    // One task owns the graph, and it ends when the call does. Both channels
    // are dropped together by the call's teardown, so whichever is noticed
    // first is the same ending.
    crate::web::spawn(async move {
        let graph = graph;
        let fed = feed_playout(speaker_rx, rate, playout);
        futures_lite::future::or(fed, async {
            mic_tx.closed().await;
        })
        .await;
        drop(graph);
    });

    Ok((mic_rx, speaker_tx))
}

/// Ask for the microphone, with the processing a call wants.
async fn open_microphone() -> Result<web_sys::MediaStream> {
    let devices = media_devices()?;
    // The three constraints a conversation needs and a recording does not.
    // Without `echoCancellation` a caller on speakers hears themselves, which
    // is the failure most often mistaken for a bad connection.
    let audio = js_sys::Object::new();
    for name in ["echoCancellation", "noiseSuppression", "autoGainControl"] {
        let _ = js_sys::Reflect::set(
            &audio,
            &wasm_bindgen::JsValue::from_str(name),
            &wasm_bindgen::JsValue::TRUE,
        );
    }
    let constraints = web_sys::MediaStreamConstraints::new();
    constraints.set_audio(&audio);

    wasm_bindgen_futures::JsFuture::from(
        devices
            .get_user_media_with_constraints(&constraints)
            .map_err(|e| anyhow!("the microphone could not be opened: {}", describe(&e)))?,
    )
    .await
    .map_err(|e| anyhow!("the microphone was refused: {}", describe(&e)))?
    .dyn_into::<web_sys::MediaStream>()
    .map_err(|_| anyhow!("the browser opened something that is not a stream"))
}

/// The page's `navigator.mediaDevices`, from a window or a worker.
fn media_devices() -> Result<web_sys::MediaDevices> {
    web_sys::window()
        .ok_or_else(|| anyhow!("no window to open a microphone from"))?
        .navigator()
        .media_devices()
        .map_err(|e| anyhow!("this browser offers no microphone: {}", describe(&e)))
}

/// Build the capture and playout halves and connect them.
fn wire(
    context: &web_sys::AudioContext,
    stream: &web_sys::MediaStream,
    rate: u32,
    mic: async_channel::Sender<Vec<i16>>,
    playout: Rc<RefCell<VecDeque<f32>>>,
) -> Result<Graph> {
    let source = context
        .create_media_stream_source(stream)
        .map_err(|e| anyhow!("the microphone could not be attached: {}", describe(&e)))?;
    let capture = context
        .create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(CHUNK, 1, 1)
        .map_err(|e| anyhow!("no capture node: {}", describe(&e)))?;

    let on_capture = {
        // The resampler carries filter history and a fractional cursor across
        // callbacks; one built per callback would click at every boundary.
        let mut down = crate::resample::Stream::new(rate, CALL_RATE);
        let mut pending: Vec<i16> = Vec::with_capacity(CALL_FRAME_SAMPLES * 2);
        let mut scratch: Vec<i16> = Vec::new();
        let mut block: Vec<i16> = Vec::new();
        Closure::<dyn FnMut(web_sys::AudioProcessingEvent)>::new(
            move |event: web_sys::AudioProcessingEvent| {
                let Ok(buffer) = event.input_buffer() else {
                    return;
                };
                let Ok(channel) = buffer.get_channel_data(0) else {
                    return;
                };
                block.clear();
                block.extend(
                    channel
                        .iter()
                        .map(|s| (s * 32767.0).round().clamp(-32768.0, 32767.0) as i16),
                );
                scratch.clear();
                down.process(&block, &mut scratch);
                pending.extend_from_slice(&scratch);
                while pending.len() >= CALL_FRAME_SAMPLES {
                    let frame: Vec<i16> = pending.drain(..CALL_FRAME_SAMPLES).collect();
                    // Dropped rather than waited for: this is the audio
                    // thread, and the newest frame is the only one worth
                    // having. A closed channel is the call ending, which the
                    // owning task notices for itself.
                    let _ = mic.try_send(frame);
                }
            },
        )
    };
    capture.set_onaudioprocess(Some(on_capture.as_ref().unchecked_ref()));

    let playout_node = context
        .create_script_processor_with_buffer_size_and_number_of_input_channels_and_number_of_output_channels(CHUNK, 0, 1)
        .map_err(|e| anyhow!("no playout node: {}", describe(&e)))?;

    let on_playout = {
        let playout = Rc::clone(&playout);
        Closure::<dyn FnMut(web_sys::AudioProcessingEvent)>::new(
            move |event: web_sys::AudioProcessingEvent| {
                let Ok(buffer) = event.output_buffer() else {
                    return;
                };
                let Ok(mut channel) = buffer.get_channel_data(0) else {
                    return;
                };
                let mut ring = playout.borrow_mut();
                for slot in channel.iter_mut() {
                    // An empty ring is a gap in the network, not an error:
                    // the peer stopped sending, or their packet is late.
                    // Silence is what a call sounds like there.
                    *slot = ring.pop_front().unwrap_or(0.0);
                }
                drop(ring);
                let _ = buffer.copy_to_channel(&channel, 0);
            },
        )
    };
    playout_node.set_onaudioprocess(Some(on_playout.as_ref().unchecked_ref()));

    source
        .connect_with_audio_node(&capture)
        .map_err(|e| anyhow!("the capture graph would not connect: {}", describe(&e)))?;
    // A ScriptProcessorNode does not run unless it reaches the destination,
    // even though nothing wants to hear the microphone locally: the capture
    // node never writes its output buffer, so what reaches the speakers from
    // this branch is zeros.
    capture
        .connect_with_audio_node(&context.destination())
        .map_err(|e| anyhow!("the capture graph would not run: {}", describe(&e)))?;
    playout_node
        .connect_with_audio_node(&context.destination())
        .map_err(|e| anyhow!("the playout graph would not connect: {}", describe(&e)))?;

    Ok(Graph {
        context: context.clone(),
        stream: stream.clone(),
        capture,
        playout: playout_node,
        _on_capture: on_capture,
        _on_playout: on_playout,
    })
}

/// Move the peer's audio into the ring the playout callback drains.
///
/// Returns when the call drops the sender, which is what ends the graph.
async fn feed_playout(
    frames: async_channel::Receiver<Vec<i16>>,
    rate: u32,
    playout: Rc<RefCell<VecDeque<f32>>>,
) {
    let mut up = crate::resample::Stream::new(CALL_RATE, rate);
    let mut scratch: Vec<i16> = Vec::new();
    let mut overran = false;
    while let Ok(frame) = frames.recv().await {
        scratch.clear();
        up.process(&frame, &mut scratch);
        let mut ring = playout.borrow_mut();
        ring.extend(scratch.iter().map(|&s| f32::from(s) / 32768.0));
        if ring.len() > PLAYOUT_CEILING {
            // The callback is not draining — a suspended context, or a tab
            // the browser has throttled. Dropping from the front keeps the
            // audio that is about to be heard rather than the audio that
            // should already have been.
            let excess = ring.len() - PLAYOUT_CEILING;
            ring.drain(..excess);
            if !overran {
                overran = true;
                warn!("the call's playout ring overran: the page's audio thread is behind");
            }
        }
    }
}

/// A `JsValue` as something worth putting in a log line.
fn describe(value: &wasm_bindgen::JsValue) -> String {
    value
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| value.as_string())
        .unwrap_or_else(|| format!("{value:?}"))
}
