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

/// How much silence is put in front of the peer's first frame.
///
/// The callback runs on a strict clock and the network does not, so a ring
/// that starts empty emits a gap on its very first callback and then again on
/// every packet that is a moment late. 60 ms is one frame of headroom, paid
/// once.
///
/// Added when that first frame arrives rather than when the graph is built,
/// and the difference is the whole of the headroom. `resume` happens here;
/// signalling and relay setup happen after, and they take far longer than
/// 60 ms — so a ring primed at construction is one the callback has already
/// drained to nothing by the time there is anything to play, leaving the
/// first real frames with exactly the margin this exists to give them. An
/// empty ring in the meantime costs nothing: the callback writes silence for
/// a missing sample, which is what a call with no audio yet sounds like.
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
    /// The node the microphone feeds, held so it can be *disconnected*.
    ///
    /// It was a local in `wire` before, which left the only thing still
    /// joined to the live stream as the one thing teardown could not reach:
    /// dropping a `MediaStreamAudioSourceNode`'s Rust handle unwires nothing,
    /// and a source still attached to a running context is a context still
    /// reading the device. That is a tab whose microphone indicator stays lit
    /// after a call that never connected.
    source: web_sys::MediaStreamAudioSourceNode,
    capture: web_sys::ScriptProcessorNode,
    playout: web_sys::ScriptProcessorNode,
    _on_capture: Closure<dyn FnMut(web_sys::AudioProcessingEvent)>,
    _on_playout: Closure<dyn FnMut(web_sys::AudioProcessingEvent)>,
    /// One per track, kept alive for as long as the graph is: a closure that
    /// has been dropped traps when the browser calls it.
    _on_ended: Vec<Closure<dyn FnMut(web_sys::Event)>>,
}

impl Drop for Graph {
    fn drop(&mut self) {
        // Detached first: a node still wired to the destination goes on being
        // called while the context closes, and `close` is asynchronous.
        self.capture.set_onaudioprocess(None);
        self.playout.set_onaudioprocess(None);
        // The source first: it is what joins the device to the context, and
        // the two nodes below are downstream of it.
        let _ = self.source.disconnect();
        let _ = self.capture.disconnect();
        let _ = self.playout.disconnect();
        let mut stopped = 0usize;
        for track in self.stream.get_tracks().iter() {
            if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                // Before `stop`, and before the closures below are dropped
                // with `self`: `stop()` is specified not to fire `ended`, but
                // a handler the browser calls after its closure has gone is a
                // trap rather than a missed event, so nothing is left armed.
                track.set_onended(None);
                track.stop();
                stopped += 1;
            }
        }
        let _ = self.context.close();
        // The count, because "the graph is closed" was a sentence this line
        // could print while releasing nothing: a stream whose tracks it could
        // not read looks exactly the same in a log as one it stopped. A zero
        // here is a microphone still running, and it names itself.
        debug!("the call's audio graph is closed ({stopped} track(s) stopped)");
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

/// Detaches nodes that were wired before `wire` failed.
///
/// A `ScriptProcessorNode` with a handler attached is a node the browser will
/// call, and `Opening`'s close is asynchronous — so a `wire` that returns
/// early drops its `Closure`s while the nodes still reference them, and the
/// next audio callback is a call into freed memory rather than a missed
/// frame. Every node gets detached here before the closures go, which is the
/// same ordering `Graph::drop` uses for the same reason.
/// Declared after the `Closure`s it protects: locals drop in reverse, so a
/// guard declared before them detaches the handlers only once the closures
/// are already gone, which is the trap rather than the fix. The relay's
/// `ChannelGuard` is the same shape for the same reason.
struct Wiring(Vec<web_sys::ScriptProcessorNode>);

impl Wiring {
    /// `Graph` detaches them from here.
    fn release(mut self) {
        self.0.clear();
    }
}

impl Drop for Wiring {
    fn drop(&mut self) {
        for node in self.0.drain(..) {
            node.set_onaudioprocess(None);
            let _ = node.disconnect();
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
    crate::call_ending::CallAudioFacts,
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
    // Empty, and primed by `feed_playout` when the peer's first frame lands;
    // see `PLAYOUT_PRIME` for why it is not primed here.
    let playout: Rc<RefCell<VecDeque<f32>>> = Rc::new(RefCell::new(VecDeque::new()));

    // What the ending is read against: whether a track went on this side, and
    // whether the engine ever received these endpoints at all. Both are
    // answered outside this graph, and without them an unplugged microphone
    // and an ordinary cancellation are both reported as the engine letting a
    // call go. See `crate::call_ending`.
    let facts = crate::call_ending::CallAudioFacts::default();

    let graph = wire(
        &context,
        &stream,
        rate,
        mic_tx.clone(),
        Rc::clone(&playout),
        facts.clone(),
    )?;

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
    let owned_facts = facts.clone();
    crate::web::spawn(async move {
        let graph = graph;
        // Which half went is not a branch — the graph closes either way — it
        // is the *name*, and the name is the whole diagnostic. An engine
        // whose driver returns without ever using its transport releases both
        // ends at the instant one that ran a whole conversation does, so the
        // teardown looks identical from here and the log is the only place
        // the difference can survive. See `crate::call_ending`.
        let ending = crate::call_ending::ending(
            feed_playout(speaker_rx, rate, playout),
            mic_tx.closed(),
            &owned_facts,
        )
        .await;
        debug!("the call's audio is ending: {}", ending.as_str());
        drop(graph);
    });

    Ok((mic_rx, speaker_tx, facts))
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

    let asked = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(|e| anyhow!("the microphone could not be opened: {}", describe(&e)))?;

    // Bounded, because the thing this waits on is a person. `getUserMedia`
    // settles when the permission prompt is answered and not before, and this
    // is awaited *inside* placing or accepting a call — an accept has already
    // consumed the offer by the time it gets here, so a hangup while the
    // prompt sits there can only be recorded as deferred while the caller
    // goes on ringing until their own timeout. The camera's prompt is bounded
    // the same way and for the same half of this reason.
    let abandoned = std::rc::Rc::new(std::cell::Cell::new(false));
    // A prompt answered after we gave up still opens the device, and the
    // stream it resolves with is one nothing here is holding — so its tracks
    // would run, with the tab's indicator on, until the page went away. The
    // same promise is awaited twice, which is what promises are for.
    {
        let abandoned = std::rc::Rc::clone(&abandoned);
        let late = asked.clone();
        crate::web::spawn(async move {
            let Ok(value) = wasm_bindgen_futures::JsFuture::from(late).await else {
                return;
            };
            if !abandoned.get() {
                return;
            }
            if let Ok(stream) = value.dyn_into::<web_sys::MediaStream>() {
                warn!("the microphone opened after the call gave up waiting for it; closing it");
                for track in stream.get_tracks().iter() {
                    if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
                        track.stop();
                    }
                }
            }
        });
    }

    let opened = wasm_bindgen_futures::JsFuture::from(asked);
    let Some(opened) = futures_lite::future::or(async move { Some(opened.await) }, async {
        after(PERMISSION_CEILING_MS).await;
        None
    })
    .await
    else {
        abandoned.set(true);
        bail!("the microphone permission prompt went unanswered");
    };

    opened
        .map_err(|e| anyhow!("the microphone was refused: {}", describe(&e)))?
        .dyn_into::<web_sys::MediaStream>()
        .map_err(|_| anyhow!("the browser opened something that is not a stream"))
}

/// How long a microphone permission prompt is waited on.
///
/// Generous, because it is a person reading a dialog: a call that gives up
/// while somebody was reaching for the mouse is the worse failure. Short
/// enough that a prompt left on screen does not hold a call open with nothing
/// happening at either end.
const PERMISSION_CEILING_MS: i32 = 30_000;

/// Resolve after `ms`, through the only clock this target has.
///
/// `tokio::time` links here and traps on the first await; the session says
/// the same thing in `exec::sleep`, which this crate has no route to.
async fn after(ms: i32) {
    let (tx, rx) = async_channel::bounded::<()>(1);
    let fire = Closure::once_into_js(move || {
        let _ = tx.try_send(());
    });
    let armed = web_sys::window().and_then(|window| {
        window
            .set_timeout_with_callback_and_timeout_and_arguments_0(fire.unchecked_ref(), ms)
            .ok()
    });
    if armed.is_none() {
        // No timer to arm means no ceiling to enforce; waiting forever on a
        // channel nothing will send to leaves the other side of the race the
        // only one that can finish, which is how this behaved before the
        // ceiling existed.
        warn!("no timer to bound the microphone permission prompt with");
    }
    let _ = rx.recv().await;
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
    facts: crate::call_ending::CallAudioFacts,
) -> Result<Graph> {
    // Taken before the capture callback moves the sender in; see the tracks'
    // `ended` handlers at the end of this function.
    let ended_mic = mic.clone();
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
                    // `force_send` rather than `try_send`: this is the audio
                    // thread and it cannot wait, and what a full queue holds
                    // is the *oldest* speech — up to four frames of it. A
                    // `try_send` drops the frame just captured and keeps
                    // those, which is the policy backwards: on recovery the
                    // peer hears stale audio, either as a burst or as latency
                    // that never comes back down if both ends then run at the
                    // same rate. Evicting the oldest is what the rest of this
                    // path does. A closed channel is the call ending, which
                    // the owning task notices for itself.
                    //
                    // Safe here and *not* on the camera's queue, which refuses
                    // its newest instead: a PCM frame stands on its own, so
                    // dropping an older one costs exactly that frame, while an
                    // H.264 picture is referenced by the ones behind it and
                    // evicting one makes the rest undecodable.
                    let _ = mic.force_send(frame);
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
    // Declared *here*, after both closures, and that placement is the whole
    // of it: locals drop in reverse, so a guard declared before them would
    // detach the handlers only once the closures it was protecting had
    // already been dropped — which is the trap rather than the fix. Nothing
    // above this line is a live node: a `ScriptProcessorNode` fires only
    // while it is connected, and the connections are all below.
    let wiring = Wiring(vec![capture.clone(), playout_node.clone()]);

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

    // A microphone can end without the call ending: unplugged, revoked in the
    // site settings, or taken by the operating system. The capture callback
    // holds the sender, so nothing else would ever close it — the call stays
    // up, the engine goes on waiting for input, and the person on the other
    // end hears silence with nothing anywhere saying why. Closing the channel
    // is the same ending the teardown uses, so it needs no second path out.
    let on_ended: Vec<Closure<dyn FnMut(web_sys::Event)>> = stream
        .get_tracks()
        .iter()
        .filter_map(|track| track.dyn_into::<web_sys::MediaStreamTrack>().ok())
        .map(|track| {
            let mic = ended_mic.clone();
            let ended_locally = facts.clone();
            let ended = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
                warn!("the call's microphone ended: its track stopped");
                // Before the close, because closing is what wakes the arm
                // that reads it.
                ended_locally.capture_ended();
                mic.close();
            });
            track.set_onended(Some(ended.as_ref().unchecked_ref()));
            ended
        })
        .collect();

    // Past every `?`: `Graph` detaches these from here.
    wiring.release();

    Ok(Graph {
        context: context.clone(),
        stream: stream.clone(),
        source,
        capture,
        playout: playout_node,
        _on_capture: on_capture,
        _on_playout: on_playout,
        _on_ended: on_ended,
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
    let mut primed = false;
    while let Ok(frame) = frames.recv().await {
        scratch.clear();
        up.process(&frame, &mut scratch);
        let mut ring = playout.borrow_mut();
        if !primed {
            // In front of the first frame, so the headroom is there when the
            // audio is. See `PLAYOUT_PRIME`.
            primed = true;
            ring.extend(std::iter::repeat_n(
                0.0,
                rate as usize * PLAYOUT_PRIME / 1000,
            ));
        }
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
