//! Voice notes and video sound, played by the browser.
//!
//! The native player owns the decode and the mixing: it pulls Opus out of an
//! OGG stream, resamples, re-times, and writes into a cpal callback. None of
//! that is available to a page — libopus is C and does not build for wasm —
//! and none of it needs to be, because a browser decodes Opus itself and has
//! a mixer already. `decodeAudioData` takes the same bytes the daemon sends
//! and hands back a buffer; an `AudioBufferSourceNode` plays it.
//!
//! What that costs is the *shape* of the API. The native player is
//! synchronous — `play` returns once the stream is running — and
//! `decodeAudioData` is a promise. So `play` here arms the decode and
//! returns, and everything that reads the clock reports the state before the
//! sound starts until it does. Every caller already tolerates that: the
//! progress bar is polled, not pushed.

use std::cell::RefCell;
use std::rc::Rc;

use log::{info, warn};
use tokio::sync::oneshot;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{AudioBuffer, AudioBufferSourceNode, AudioContext};

use crate::player::PlayerError;

/// Where the clip's state lives.
///
/// Shared with the callbacks the browser will run — the decode's completion
/// and the node's `ended` — which is why it is an `Rc<RefCell<_>>` rather
/// than fields on the player: those callbacks outlive the call that armed
/// them, and one of them fires after the user has already moved on.
#[derive(Default)]
struct Playing {
    /// The node that is making sound, if any. Kept so pausing, stopping and
    /// seeking have something to act on.
    node: Option<AudioBufferSourceNode>,
    buffer: Option<AudioBuffer>,
    /// Where the context's clock stood when the current run started, and how
    /// far into the clip that run began. Together they answer "how far in are
    /// we" without the node being asked, which it cannot be.
    started_at: f64,
    offset: f64,
    duration: f64,
    is_playing: bool,
    finished: bool,
    completion: Option<oneshot::Sender<()>>,
}

/// Audio player for PTT voice messages.
pub struct AudioPlayer {
    /// Created lazily and kept: a page may open several contexts before the
    /// user has interacted with it, and every one of them starts suspended.
    /// One context, resumed on the first play, is what a browser expects.
    context: Option<AudioContext>,
    state: Rc<RefCell<Playing>>,
    speed: f32,
}

impl Default for AudioPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPlayer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            context: None,
            state: Rc::new(RefCell::new(Playing::default())),
            speed: 1.0,
        }
    }

    /// How fast the next clip is played.
    ///
    /// The browser's `playbackRate` resamples rather than time-stretching, so
    /// a voice note at 2× is the same voice higher. The native player
    /// preserves pitch and this does not; it is the one audible difference
    /// between the two, and it is the browser's own control rather than a
    /// second implementation of one.
    pub fn set_speed(&mut self, speed: f32) {
        self.speed = speed.clamp(0.5, 3.0);
        if let Some(node) = self.state.borrow().node.as_ref() {
            node.playback_rate().set_value(self.speed);
        }
    }

    #[must_use]
    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// A receiver that fires when the clip runs to its end.
    pub fn on_complete(&mut self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.state.borrow_mut().completion = Some(tx);
        rx
    }

    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.state.borrow().is_playing
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state.borrow().finished
    }

    #[must_use]
    pub fn progress(&self) -> f32 {
        let total = self.total_secs();
        if total <= 0.0 {
            return 0.0;
        }
        (self.elapsed_secs() / total).clamp(0.0, 1.0)
    }

    #[must_use]
    pub fn elapsed_secs(&self) -> f32 {
        let state = self.state.borrow();
        let elapsed = if state.is_playing {
            let now = self
                .context
                .as_ref()
                .map_or(0.0, AudioContext::current_time);
            state.offset + (now - state.started_at) * f64::from(self.speed)
        } else {
            state.offset
        };
        (elapsed.clamp(0.0, state.duration)) as f32
    }

    #[must_use]
    pub fn total_secs(&self) -> f32 {
        (self.state.borrow().duration / f64::from(self.speed).max(0.001)) as f32
    }

    /// Jump to a fraction of the clip.
    ///
    /// A source node cannot be moved: it is started once with an offset and
    /// then only stopped. So a seek is a new node from the same buffer, which
    /// is what the browser expects and why nothing here tries to hold one
    /// open. Refused once the clip has finished, for the same reason the
    /// native player refuses it — the caller starts it over instead.
    pub fn seek(&self, fraction: f32) -> bool {
        if self.state.borrow().finished {
            return false;
        }
        let Some(context) = self.context.as_ref() else {
            return false;
        };
        let (buffer, was_playing, duration) = {
            let state = self.state.borrow();
            (state.buffer.clone(), state.is_playing, state.duration)
        };
        let Some(buffer) = buffer else {
            return false;
        };
        let offset = (f64::from(fraction.clamp(0.0, 1.0))) * duration;
        stop_node(&self.state);
        self.state.borrow_mut().offset = offset;
        if was_playing {
            start(context, &buffer, &self.state, self.speed, offset);
        }
        true
    }

    /// Play an Opus/OGG voice note.
    ///
    /// # Errors
    ///
    /// Only for the things that fail before the decode: no audio context to
    /// be had, or no bytes to decode. A decode that fails afterwards is
    /// reported in the log and leaves the player idle, because by then the
    /// caller has already been told the play was accepted.
    pub fn play(&mut self, ogg_data: Vec<u8>) -> Result<(), PlayerError> {
        if ogg_data.is_empty() {
            return Err(PlayerError::EmptyAudio);
        }
        let context = self.context()?;
        self.stop();

        // A page that has not been clicked yet has a suspended context, and a
        // node started on one makes no sound and never fires `ended`.
        let _ = context.resume();

        let array = js_sys::Uint8Array::from(ogg_data.as_slice());
        let decoding = context
            .decode_audio_data(&array.buffer())
            .map_err(|e| PlayerError::DecodeError(format!("{e:?}")))?;

        let state = Rc::clone(&self.state);
        let context = context.clone();
        let speed = self.speed;
        spawn_local(async move {
            match JsFuture::from(decoding).await {
                Ok(decoded) => {
                    let Ok(buffer) = decoded.dyn_into::<AudioBuffer>() else {
                        warn!("the browser decoded something that is not an audio buffer");
                        return;
                    };
                    info!("decoded {:.1}s of audio", buffer.duration());
                    {
                        let mut state = state.borrow_mut();
                        state.duration = buffer.duration();
                        state.offset = 0.0;
                        state.finished = false;
                        state.buffer = Some(buffer.clone());
                    }
                    start(&context, &buffer, &state, speed, 0.0);
                }
                Err(e) => warn!("the browser could not decode this clip: {e:?}"),
            }
        });
        Ok(())
    }

    /// Play raw f32 PCM samples at the given rate.
    ///
    /// Always at 1×, for the same reason the native player is: this is a
    /// video's audio track and the picture is not being re-timed.
    ///
    /// # Errors
    ///
    /// No audio context, no samples, or a buffer the browser refused to
    /// allocate.
    pub fn play_samples(
        &mut self,
        samples: Vec<f32>,
        src_sample_rate: u32,
    ) -> Result<(), PlayerError> {
        if samples.is_empty() {
            return Err(PlayerError::EmptyAudio);
        }
        let context = self.context()?;
        self.stop();
        let _ = context.resume();

        let frames = u32::try_from(samples.len()).unwrap_or(u32::MAX);
        let buffer = context
            .create_buffer(1, frames, src_sample_rate as f32)
            .map_err(|e| PlayerError::DeviceError(format!("{e:?}")))?;
        buffer
            .copy_to_channel(&samples, 0)
            .map_err(|e| PlayerError::DeviceError(format!("{e:?}")))?;

        {
            let mut state = self.state.borrow_mut();
            state.duration = buffer.duration();
            state.offset = 0.0;
            state.finished = false;
            state.buffer = Some(buffer.clone());
        }
        // 1×, not `self.speed`: see the doc comment.
        start(&context, &buffer, &self.state, 1.0, 0.0);
        Ok(())
    }

    /// Stop and forget the clip.
    pub fn stop(&mut self) {
        stop_node(&self.state);
        let mut state = self.state.borrow_mut();
        state.buffer = None;
        state.offset = 0.0;
        state.duration = 0.0;
        state.finished = false;
    }

    /// Stop making sound, keeping the position.
    pub fn pause(&mut self) {
        if !self.state.borrow().is_playing {
            return;
        }
        let now = self
            .context
            .as_ref()
            .map_or(0.0, AudioContext::current_time);
        let resume_at = {
            let state = self.state.borrow();
            (state.offset + (now - state.started_at) * f64::from(self.speed)).min(state.duration)
        };
        stop_node(&self.state);
        self.state.borrow_mut().offset = resume_at;
    }

    /// Carry on from where [`pause`](Self::pause) left off.
    pub fn resume(&mut self) {
        let (buffer, offset, playing, finished) = {
            let state = self.state.borrow();
            (
                state.buffer.clone(),
                state.offset,
                state.is_playing,
                state.finished,
            )
        };
        if playing || finished {
            return;
        }
        let (Some(context), Some(buffer)) = (self.context.as_ref(), buffer) else {
            return;
        };
        let _ = context.resume();
        start(context, &buffer, &self.state, self.speed, offset);
    }

    /// The one context this player uses, made on first need.
    ///
    /// Handed back by value rather than by reference: an `AudioContext` is a
    /// handle to a JS object, so cloning is a refcount, and returning a
    /// borrow of `self` would stop the caller from touching anything else on
    /// the player while it held one.
    fn context(&mut self) -> Result<AudioContext, PlayerError> {
        if let Some(context) = &self.context {
            return Ok(context.clone());
        }
        let context =
            AudioContext::new().map_err(|e| PlayerError::DeviceError(format!("{e:?}")))?;
        self.context = Some(context.clone());
        Ok(context)
    }
}

/// Start a node from `offset` and wire up what happens when it ends.
fn start(
    context: &AudioContext,
    buffer: &AudioBuffer,
    state: &Rc<RefCell<Playing>>,
    speed: f32,
    offset: f64,
) {
    let Ok(node) = context.create_buffer_source() else {
        warn!("the browser refused a source node");
        return;
    };
    node.set_buffer(Some(buffer));
    node.playback_rate().set_value(speed);
    if node
        .connect_with_audio_node(&context.destination())
        .is_err()
    {
        warn!("the browser refused to connect the source node");
        return;
    }

    {
        // Fires on a natural end *and* on a stop we asked for, so the handler
        // checks whether the node it is about is still the one on the player:
        // a seek replaces the node, and its predecessor's `ended` would
        // otherwise report the clip as finished a moment after it restarted.
        let state = Rc::clone(state);
        let ended = Closure::<dyn FnMut()>::new(move || {
            let completion = {
                let mut state = state.borrow_mut();
                if !state.is_playing {
                    return;
                }
                state.is_playing = false;
                state.finished = true;
                state.offset = state.duration;
                state.node = None;
                state.completion.take()
            };
            if let Some(tx) = completion {
                let _ = tx.send(());
            }
        });
        // `addEventListener` rather than the `onended` property, which
        // web-sys deprecates.
        if node
            .add_event_listener_with_callback("ended", ended.as_ref().unchecked_ref())
            .is_err()
        {
            warn!("the browser refused an ended listener");
        }
        // Dropped when the node is: the browser holds the only other
        // reference, and a freed callback on a live node is a crash.
        ended.forget();
    }

    if let Err(e) = node.start_with_when_and_grain_offset(0.0, offset) {
        warn!("the browser refused to start playback: {e:?}");
        return;
    }

    let mut state = state.borrow_mut();
    state.started_at = context.current_time();
    state.offset = offset;
    state.is_playing = true;
    state.finished = false;
    state.node = Some(node);
}

/// Silence whatever is playing, without touching the position.
fn stop_node(state: &Rc<RefCell<Playing>>) {
    let node = {
        let mut state = state.borrow_mut();
        state.is_playing = false;
        state.node.take()
    };
    if let Some(node) = node {
        // The listener checks `is_playing` and this cleared it, so the stop
        // below fires `ended` into a handler that returns immediately —
        // which is why nothing has to be unregistered here.
        //
        // Through the base class: `stop` is defined on
        // `AudioScheduledSourceNode`, and web-sys deprecates the copies it
        // generated onto the subclasses.
        let scheduled: &web_sys::AudioScheduledSourceNode = node.as_ref();
        let _ = scheduled.stop();
        let _ = node.disconnect();
    }
}
