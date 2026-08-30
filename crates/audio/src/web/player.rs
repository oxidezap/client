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
struct Playing {
    /// The node that is making sound, if any. Kept so pausing, stopping and
    /// seeking have something to act on.
    node: Option<AudioBufferSourceNode>,
    /// The `ended` listener for that node, held so it dies with it.
    ///
    /// `Closure::forget` would hand it to the JS heap for the life of the
    /// page, and a node is replaced on every play, seek and resume — so the
    /// count would grow with how much the user scrubs.
    ended: Option<Closure<dyn FnMut()>>,
    buffer: Option<AudioBuffer>,
    /// Which run of the player the state describes.
    ///
    /// Bumped by everything that supersedes what came before. A decode is a
    /// promise: a second `play` inside its window would otherwise have the
    /// first one's buffer install itself on top when it resolved, leaving two
    /// nodes connected to the destination and only the last one reachable
    /// through `stop`. The `ended` listener compares it too, so the node a
    /// seek replaced cannot report the clip finished.
    generation: u64,
    /// Where the context's clock stood when the current run started, and how
    /// far into the clip that run began. Together they answer "how far in are
    /// we" without the node being asked, which it cannot be.
    started_at: f64,
    offset: f64,
    /// What the node that is playing is *actually* running at.
    ///
    /// Not `Player::speed`, which is the voice-note speed the person chose
    /// and outlives any one clip. A video's audio always starts at 1×, so
    /// with a 2× voice-note setting still in force the elapsed arithmetic
    /// below counted every second of video twice — pausing five seconds in
    /// recorded ten.
    rate: f64,
    /// Whether this clip is one the speed control applies to.
    ///
    /// A voice note is; a video's soundtrack is not, because the picture it
    /// belongs to plays at one speed and nothing here can change that.
    ///
    /// It is a property of the clip rather than a number snapshotted at
    /// start, and that distinction is load-bearing: a seek and a resume both
    /// *restart* the node, and asking `rate` what to restart at would replay
    /// a voice note at whatever it was going at when it stopped, ignoring a
    /// speed the person changed while it was paused. Asking this instead
    /// gives the voice note the current setting and the video its 1×.
    follows_speed: bool,
    /// The speed control's current setting, shared with a decode in flight.
    ///
    /// The same number as `Player::speed`, kept here because a decode is a
    /// promise: `play` used to capture the speed on the way in, so a note
    /// switched to 2× while `decodeAudioData` was still working started at 1×
    /// under a control already reading 2×.
    chosen: f32,
    duration: f64,
    is_playing: bool,
    finished: bool,
    /// Where a seek asked to be while the clip was still decoding, as a
    /// fraction — the duration is not known until the buffer is.
    ///
    /// `decodeAudioData` is a promise, so a caller that starts a clip and
    /// immediately positions it (which is what changing the playback speed
    /// does: it restarts the note and puts it back where it was) is acting on
    /// a player that has nothing loaded yet. Without somewhere to record the
    /// intent, both calls did nothing and the note began again from zero.
    pending_seek: Option<f32>,
    /// Whether that caller also asked for it to be paused. Same reason: a
    /// note paused when the speed changed would otherwise start playing by
    /// itself once the decode landed.
    pending_pause: bool,
    /// Whether a decode is in flight.
    ///
    /// Recorded rather than inferred. It was read off "no buffer, not
    /// finished, and something has run" — which every one of those is true of
    /// a player that has simply been *stopped*, so the predicate stayed true
    /// for the rest of the tab: a pause was banked for a clip nobody was
    /// loading, and the repaint tick that follows playback never wound down.
    /// The honest question has one answer and it is this flag.
    decoding: bool,
    completion: Option<oneshot::Sender<()>>,
}

impl Default for Playing {
    /// Hand-written for one field. `rate` is a multiplier, and a derived
    /// `0.0` would make every elapsed calculation return the offset it
    /// started from — a progress bar that never moves — for the window
    /// between construction and the first `start`.
    fn default() -> Self {
        Self {
            node: None,
            ended: None,
            buffer: None,
            generation: 0,
            started_at: 0.0,
            offset: 0.0,
            rate: 1.0,
            follows_speed: true,
            chosen: 1.0,
            duration: 0.0,
            is_playing: false,
            finished: false,
            pending_seek: None,
            pending_pause: false,
            decoding: false,
            completion: None,
        }
    }
}

impl Playing {
    /// Whether a decode is in flight — nothing loaded, but a run underway.
    fn is_loading(&self) -> bool {
        self.decoding
    }
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
        // Bank what has played at the old rate before the new one applies.
        // `offset` is measured in the clip's own seconds, so without this the
        // time already played would be re-scaled by a rate it was not played
        // at, and the progress bar would jump on every speed change.
        if self.state.borrow().is_playing {
            let now = self
                .context
                .as_ref()
                .map_or(0.0, AudioContext::current_time);
            let mut state = self.state.borrow_mut();
            state.offset += (now - state.started_at) * state.rate;
            state.started_at = now;
        }
        self.speed = speed.clamp(0.5, 3.0);
        let mut state = self.state.borrow_mut();
        // Before the early return below: a video's soundtrack does not follow
        // the control, but the setting it is not following is still the one a
        // voice note decoding right now has to start at.
        state.chosen = self.speed;
        // A video's soundtrack keeps its 1× while this is playing, the same
        // as it keeps it across a restart. The setting is still recorded, and
        // still applies to the next voice note.
        if !state.follows_speed {
            return;
        }
        if let Some(node) = state.node.as_ref() {
            node.playback_rate().set_value(self.speed);
            state.rate = f64::from(self.speed);
        }
    }

    #[must_use]
    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// What a clip should be started or restarted at.
    ///
    /// The speed control where it applies, and 1× where it does not. Every
    /// `start` goes through this rather than through `self.speed`, which is
    /// what stops a video's soundtrack from being restarted at a voice note's
    /// rate by a seek or a resume.
    fn rate_now(&self) -> f32 {
        if self.state.borrow().follows_speed {
            self.speed
        } else {
            1.0
        }
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

    /// Whether this clip is playing, or is going to as soon as it can.
    ///
    /// What a play/pause control has to ask. `is_playing` alone is false for
    /// the whole of a decode, so a second tap on a note that had not started
    /// yet was read as "resume" — which did nothing, and the decode then
    /// started the note the user had just asked to stop.
    #[must_use]
    pub fn is_active(&self) -> bool {
        let state = self.state.borrow();
        state.is_playing || (state.is_loading() && !state.pending_pause)
    }

    /// Whether a clip has been accepted but is not making sound yet.
    ///
    /// `play` returns while `decodeAudioData` is still a promise, so there is
    /// a stretch where the user has pressed play and `is_playing` is false.
    /// Anything that follows playback has to treat that as active or it
    /// stops before the clip it was following has begun.
    #[must_use]
    pub fn is_loading(&self) -> bool {
        self.state.borrow().is_loading()
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state.borrow().finished
    }

    /// Take the browser's permission to make sound, while it is still being
    /// offered.
    ///
    /// An `AudioContext` starts suspended and a browser only resumes one
    /// under a *transient user activation* — the moment after a click, which
    /// expires in seconds. A voice note that is not cached yet is downloaded
    /// first, so by the time `play` ran the activation was long gone: the
    /// context stayed suspended, the node was still marked playing, and the
    /// note was silent with an `ended` that never came.
    ///
    /// So the gesture calls this, synchronously, before it awaits anything.
    /// The context is created once and kept, and resuming an already-running
    /// one is free.
    pub fn unlock(&mut self) {
        if let Ok(context) = self.context() {
            let _ = context.resume();
        }
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
            state.offset + (now - state.started_at) * state.rate
        } else {
            state.offset
        };
        (elapsed.clamp(0.0, state.duration)) as f32
    }

    /// How long the clip is, in its own seconds.
    ///
    /// Not divided by the speed. `elapsed_secs` already reports a position on
    /// the clip's timeline — it scales wall time *up* by the rate — and
    /// `seek` maps its fraction onto the same one. Dividing here made
    /// `progress` reach 1.0 halfway through a note played at 2×, and handed
    /// the scrub bar a fraction that meant something different from the one
    /// the seek would read.
    #[must_use]
    pub fn total_secs(&self) -> f32 {
        self.state.borrow().duration as f32
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
            // Still decoding: remember where this asked to be, and let the
            // decode apply it.
            let mut state = self.state.borrow_mut();
            if state.is_loading() {
                state.pending_seek = Some(fraction.clamp(0.0, 1.0));
                return true;
            }
            return false;
        };
        let offset = (f64::from(fraction.clamp(0.0, 1.0))) * duration;
        stop_node(&self.state);
        self.state.borrow_mut().offset = offset;
        if was_playing {
            start(context, &buffer, &self.state, self.rate_now(), offset);
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

        // After `stop` above, which cleared it: this run is the one decoding.
        self.state.borrow_mut().decoding = true;

        let state = Rc::clone(&self.state);
        let context = context.clone();
        // The run this decode belongs to. `stop` above has already bumped it,
        // so anything armed before this call is superseded.
        let generation = self.state.borrow().generation;
        spawn_local(async move {
            match JsFuture::from(decoding).await {
                Ok(decoded) => {
                    // The user moved on: a different note, or none at all.
                    // Installing this buffer now would start a node nothing
                    // holds a handle to, audible until it ran out.
                    if state.borrow().generation != generation {
                        log::debug!("dropping a decode the user moved on from");
                        return;
                    }
                    let Ok(buffer) = decoded.dyn_into::<AudioBuffer>() else {
                        warn!("the browser decoded something that is not an audio buffer");
                        give_up(&state);
                        return;
                    };
                    info!("decoded {:.1}s of audio", buffer.duration());
                    // What was asked for while this was still a promise. The
                    // speed control restarts the clip and then puts it back
                    // where it was, and both of those arrive before there is
                    // anything to put back.
                    let (offset, stay_paused) = {
                        let mut state = state.borrow_mut();
                        state.decoding = false;
                        state.duration = buffer.duration();
                        state.finished = false;
                        state.buffer = Some(buffer.clone());
                        let offset = state
                            .pending_seek
                            .take()
                            .map_or(0.0, |fraction| f64::from(fraction) * state.duration);
                        state.offset = offset;
                        (offset, std::mem::take(&mut state.pending_pause))
                    };
                    // Read here rather than captured on the way in: this is
                    // after the decode, and the speed control may have moved
                    // while it ran. `follows_speed` is what keeps a video's
                    // soundtrack at 1× either way.
                    let speed = {
                        let state = state.borrow();
                        if state.follows_speed {
                            state.chosen
                        } else {
                            1.0
                        }
                    };
                    start(&context, &buffer, &state, speed, offset);
                    if stay_paused {
                        // Started and stopped rather than never started: the
                        // node is what carries the position, and a pause is
                        // "here, not playing" rather than "nowhere".
                        stop_node(&state);
                        state.borrow_mut().offset = offset;
                    }
                }
                Err(e) => {
                    warn!("the browser could not decode this clip: {e:?}");
                    // The same check the success path makes, and for a
                    // sharper reason: `give_up` finishes the run and takes
                    // the completion sender, so an abandoned clip's failure
                    // would end the clip playing *now* and leave its node
                    // running with nothing holding a handle to it.
                    if state.borrow().generation != generation {
                        log::debug!("dropping a failed decode the user moved on from");
                        return;
                    }
                    give_up(&state);
                }
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
            // Not the speed control's business, now and on every later seek
            // and resume: see `follows_speed`.
            state.follows_speed = false;
        }
        // 1×, not `self.speed`: see the doc comment.
        start(&context, &buffer, &self.state, 1.0, 0.0);
        Ok(())
    }

    /// Stop and forget the clip.
    pub fn stop(&mut self) {
        stop_node(&self.state);
        let mut state = self.state.borrow_mut();
        // Anything still decoding was for the clip being forgotten.
        state.generation = state.generation.wrapping_add(1);
        state.decoding = false;
        state.buffer = None;
        state.offset = 0.0;
        state.duration = 0.0;
        state.finished = false;
        state.pending_seek = None;
        state.pending_pause = false;
        // Back to the default, because both entry points come through here
        // first: whatever the last clip was, the next one is a voice note
        // unless it says otherwise.
        state.follows_speed = true;
        state.rate = 1.0;
    }

    /// Stop making sound, keeping the position.
    pub fn pause(&mut self) {
        if !self.state.borrow().is_playing {
            // Same as `seek`: a pause asked for while the clip is decoding
            // has to be remembered, or the note starts playing on its own the
            // moment the buffer lands.
            let mut state = self.state.borrow_mut();
            if state.is_loading() {
                state.pending_pause = true;
            }
            return;
        }
        let now = self
            .context
            .as_ref()
            .map_or(0.0, AudioContext::current_time);
        let resume_at = {
            let state = self.state.borrow();
            (state.offset + (now - state.started_at) * state.rate).min(state.duration)
        };
        stop_node(&self.state);
        self.state.borrow_mut().offset = resume_at;
    }

    /// Carry on from where [`pause`](Self::pause) left off.
    pub fn resume(&mut self) {
        {
            // Still a promise: there is nothing to start, but the intent has
            // to be recorded or a pause banked a moment ago would still be
            // honoured when the buffer lands. `pause` writes this flag from
            // the same place; this is the other half of it.
            let mut state = self.state.borrow_mut();
            if state.is_loading() {
                state.pending_pause = false;
                return;
            }
        }
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
        start(context, &buffer, &self.state, self.rate_now(), offset);
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

/// A decode that produced nothing usable, reported to whoever is waiting.
///
/// The caller was told the play was accepted — `play` returns before the
/// decode resolves — so it has already marked the note as the active one and
/// is holding the completion receiver. Logging alone left that receiver
/// unresolved and the player with no buffer, so every later tap called
/// `resume`, which does nothing, and the note stayed stuck until something
/// else was selected. Firing completion is what releases it.
fn give_up(state: &Rc<RefCell<Playing>>) {
    let completion = {
        let mut state = state.borrow_mut();
        state.is_playing = false;
        state.decoding = false;
        state.finished = true;
        state.buffer = None;
        state.pending_seek = None;
        state.pending_pause = false;
        state.completion.take()
    };
    if let Some(tx) = completion {
        let _ = tx.send(());
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
    // Every failure below goes through `give_up`, for the reason written on
    // it: `play` returned before any of this ran, so the caller is already
    // holding a completion receiver and showing the note as the active one.
    // Logging and returning left that receiver unresolved and the player with
    // no node, so every later tap called `resume`, which does nothing, and
    // the note stayed stuck until something else was selected.
    let Ok(node) = context.create_buffer_source() else {
        warn!("the browser refused a source node");
        give_up(state);
        return;
    };
    node.set_buffer(Some(buffer));
    node.playback_rate().set_value(speed);
    state.borrow_mut().rate = f64::from(speed);
    if node
        .connect_with_audio_node(&context.destination())
        .is_err()
    {
        warn!("the browser refused to connect the source node");
        give_up(state);
        return;
    }

    // This run, as of now. A seek and a fresh play both replace the node, and
    // the old node's `ended` fires *after* the new one is already playing —
    // so the listener has to know which run it belongs to. Comparing the
    // shared "is playing" flag is not enough: it is true again by then, and
    // scrubbing would report the clip finished.
    let generation = {
        let mut state = state.borrow_mut();
        state.generation = state.generation.wrapping_add(1);
        state.generation
    };

    let ended = {
        let state = Rc::clone(state);
        Closure::<dyn FnMut()>::new(move || {
            let completion = {
                let mut state = state.borrow_mut();
                if state.generation != generation || !state.is_playing {
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
        })
    };
    // `addEventListener` rather than the `onended` property, which web-sys
    // deprecates.
    // Fatal, where it used to be a warning that carried on. `ended` is the
    // only thing that fires completion for a run that plays to its end, so
    // starting without it makes a clip that sounds correct and leaves the UI
    // showing it as playing for good — which is worse than not playing it.
    if node
        .add_event_listener_with_callback("ended", ended.as_ref().unchecked_ref())
        .is_err()
    {
        warn!("the browser refused an ended listener");
        give_up(state);
        return;
    }

    if let Err(e) = node.start_with_when_and_grain_offset(0.0, offset) {
        warn!("the browser refused to start playback: {e:?}");
        give_up(state);
        return;
    }

    let mut state = state.borrow_mut();
    state.started_at = context.current_time();
    state.offset = offset;
    state.is_playing = true;
    state.finished = false;
    state.node = Some(node);
    // Held rather than forgotten, so it dies with the node it listens to.
    state.ended = Some(ended);
}

/// Silence whatever is playing, without touching the position.
fn stop_node(state: &Rc<RefCell<Playing>>) {
    let (node, listener) = {
        let mut state = state.borrow_mut();
        state.is_playing = false;
        (state.node.take(), state.ended.take())
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
        // Unregistered before the stop, so the closure can be dropped below
        // without the browser holding a reference to freed memory.
        if let Some(listener) = &listener {
            let _ = node
                .remove_event_listener_with_callback("ended", listener.as_ref().unchecked_ref());
        }
        let _ = scheduled.stop();
        let _ = node.disconnect();
    }
    drop(listener);
}
