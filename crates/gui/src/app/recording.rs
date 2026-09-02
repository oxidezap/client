//! Voice-message recording: capture, encode and send.
//!
//! The microphone, the state machine around it and the timer that draws the
//! meter are an entity of their own — [`Recorder`] — and what stayed on the
//! app is the three actions a person can take. The line between them is the
//! one the split is for: the [`Recorder`] owns the device and can say what it
//! did, and everything that needs a chat, a draft or a session is above it.
//!
//! The meter's tick is the clearest thing the split buys. It ran ten times a
//! second and ended in a `cx.notify()` on the app, so a voice note being
//! recorded redrew the chat list, the sidebar and the header along with the
//! two things that had actually moved. The composer is a view of its own and
//! already repaints itself when the level changes, so the tick now says so to
//! the composer and to nothing else.

use gpui::{Context, Entity, Task, WeakEntity};

use super::*;

/// How often the recording panel is repainted while capture runs.
///
/// Ten a second: fast enough that the meter follows a voice, slow enough that
/// it is nothing next to the audio callback already running.
const RECORDING_TICK_MS: u64 = 100;

/// What closing the microphone produced.
pub(super) enum Stopped {
    /// Nothing was bound to it, so there is nothing to send. Already
    /// cancelled: the device has to be released either way.
    Nowhere,
    /// The device would not stop, with the sentence to show for it. Also
    /// already cancelled — see [`Recorder::stop`].
    Refused(String),
    /// What was captured, and which recording it belongs to.
    Captured {
        target: RecordingTarget,
        recording: oxidezap_audio::Recording,
        epoch: usize,
    },
}

/// The microphone, and what it is being held open for.
pub(super) struct Recorder {
    device: AudioRecorder,
    state: RecordingState,
    /// Chat the current PTT recording started in; the note is sent there even
    /// if the user switches chats before stopping.
    target: Option<RecordingTarget>,
    /// Which recording the encode still in flight belongs to.
    ///
    /// Encoding runs detached on the background pool and nothing can stop it,
    /// so cancelling is not a matter of aborting the work but of disowning
    /// its result. Bumped by [`Self::cancel`]; a completion whose epoch no
    /// longer matches is dropped rather than sent.
    epoch: usize,
    /// Repaints the recording panel's clock and level meter. Only alive while
    /// the microphone is.
    tick: Option<Task<()>>,
}

impl Recorder {
    pub(super) fn new() -> Self {
        Self {
            device: AudioRecorder::new(),
            state: RecordingState::default(),
            target: None,
            epoch: 0,
            tick: None,
        }
    }

    pub(super) fn state(&self) -> RecordingState {
        self.state
    }

    pub(super) fn is_recording(&self) -> bool {
        self.state.is_recording()
    }

    /// Which recording an encode in flight has to still belong to.
    pub(super) fn epoch(&self) -> usize {
        self.epoch
    }

    /// Open the microphone for `target`, or say why it stayed shut.
    ///
    /// The `Err` is written for a reader, because every one of these is a
    /// press that otherwise does nothing at all: the composer stays as it
    /// was, with the reason only in the log.
    ///
    /// Takes both the composer it has in hand *and* the window it belongs to:
    /// the first is what is told, right now, that a panel is up, and the
    /// second is what the meter's timer asks again on every tick, because a
    /// composer built after the microphone opened still has a meter to draw.
    pub(super) fn open(
        &mut self,
        target: RecordingTarget,
        composer: Option<Entity<InputAreaView>>,
        app: WeakEntity<WhatsAppApp>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        // Refused where nothing can come of it. The composer already draws
        // the microphone disabled there, so this is the keyboard route and
        // anything else that reaches the action directly.
        if !oxidezap_audio::can_record() {
            warn!("this build cannot record a voice note");
            return Err("Voice messages cannot be recorded in this browser.".to_string());
        }
        if let Err(e) = self.device.init() {
            error!("Failed to initialize audio recorder: {}", e);
            return Err("The microphone could not be opened.".to_string());
        }
        if let Err(e) = self.device.start() {
            error!("Failed to start recording: {}", e);
            return Err("Recording could not be started.".to_string());
        }

        self.state = RecordingState::Recording;
        self.target = Some(target);
        self.show(&composer, cx);
        self.ensure_tick(app, cx);
        Ok(())
    }

    /// Close the microphone and hand back what it captured.
    pub(super) fn stop(
        &mut self,
        composer: Option<Entity<InputAreaView>>,
        cx: &mut Context<Self>,
    ) -> Stopped {
        let Some(target) = self.take_target() else {
            warn!("No recording chat, cancelling recording");
            self.cancel(composer, cx);
            return Stopped::Nowhere;
        };

        self.state = RecordingState::Processing;
        self.show(&composer, cx);

        match self.device.stop() {
            Ok(recording) => Stopped::Captured {
                target,
                recording,
                epoch: self.epoch,
            },
            Err(e) => {
                error!("Failed to stop recording: {}", e);
                // Said *and* acted on. Through the cancel, which is the only
                // thing that releases the device: setting the state to idle
                // alone left the capture running with `is_recording` false and
                // the panel gone, so no control on screen could close the
                // microphone after that — a notice about a microphone that is
                // still open is the worse half of that bug, not a fix for it.
                self.cancel(composer, cx);
                Stopped::Refused("The recording could not be stopped.".to_string())
            }
        }
    }

    /// Back to idle after an encode that ended, however it ended.
    pub(super) fn settle(
        &mut self,
        composer: Option<Entity<InputAreaView>>,
        cx: &mut Context<Self>,
    ) {
        self.idle();
        self.show(&composer, cx);
    }

    /// Release the device and disown whatever it was for.
    pub(super) fn cancel(
        &mut self,
        composer: Option<Entity<InputAreaView>>,
        cx: &mut Context<Self>,
    ) {
        self.disown();
        self.show(&composer, cx);
    }

    /// Where the note this recording is for was bound, taken so a second stop
    /// has nowhere to send. See [`RecordingTarget`].
    fn take_target(&mut self) -> Option<RecordingTarget> {
        self.target.take()
    }

    /// Back to idle, keeping the epoch: the encode this settles is the one
    /// that just ended, and bumping here would have every successful send
    /// discard the recording after it.
    fn idle(&mut self) {
        self.state = RecordingState::Idle;
    }

    /// Release the device and disown whatever it was for.
    ///
    /// The epoch bump is the disowning. Encoding runs detached and nothing
    /// can stop it, so cancelling is not a matter of aborting the work but of
    /// having the completion ask on the way back whether the recording it was
    /// started for is still the current one — and this is the answer that
    /// says no. Apart from the composer it tells, so a test can drive the
    /// half that decides.
    fn disown(&mut self) {
        self.device.cancel();
        self.idle();
        self.target = None;
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Tell the composer whether it is drawing a recording panel.
    fn show(&self, composer: &Option<Entity<InputAreaView>>, cx: &mut Context<Self>) {
        if let Some(composer) = composer {
            let is_recording = self.is_recording();
            composer.update(cx, |view, cx| view.set_recording(is_recording, cx));
        }
    }

    /// Repaint the recording panel while capture is running.
    ///
    /// Without it the panel drew once and then sat there: the timer is derived
    /// from an `Instant`, which is only as current as the last repaint, and
    /// the meter had no source at all. This is that source — the recorder's
    /// own buffer, read at a rate a person can follow, and stopped the moment
    /// recording does so an idle window wakes for nothing.
    ///
    /// The repaint it asks for is the composer's, which is the view that
    /// draws both the clock and the meter. Nothing else on screen moves ten
    /// times a second because somebody is talking — which is what this cost
    /// before the split, when the tick ended in a `cx.notify()` on the app.
    ///
    /// The composer is looked up on every pass rather than captured, for the
    /// two reasons the old lookup had: one built after the microphone opened
    /// still gets a meter, and a window that has gone ends the timer instead
    /// of writing into a handle nobody is drawing.
    fn ensure_tick(&mut self, app: WeakEntity<WhatsAppApp>, cx: &mut Context<Self>) {
        if self.tick.is_some() || !self.is_recording() {
            return;
        }
        self.tick = Some(cx.spawn(async move |me: WeakEntity<Self>, cx| {
            loop {
                crate::platform::sleep(std::time::Duration::from_millis(RECORDING_TICK_MS)).await;
                let level = me.update(cx, |recorder, _| {
                    recorder.is_recording().then(|| recorder.device.level())
                });
                let Ok(Some(level)) = level else {
                    break;
                };
                let Ok(composer) = app.update(cx, |app, _| app.input_area.clone()) else {
                    break;
                };
                // The clock lives in the view and is read from an `Instant`,
                // so this repaint is what advances it as well as the meter.
                if let Some(composer) = composer {
                    composer.update(cx, |view, cx| view.set_level(level, cx));
                }
            }
            let _ = me.update(cx, |recorder, _| recorder.tick = None);
        }));
    }
}

impl WhatsAppApp {
    /// Start audio recording for PTT
    pub fn start_recording(&mut self, cx: &mut Context<Self>) {
        let Some(jid) = self.selected_chat.clone() else {
            warn!("Cannot record: no chat selected");
            return;
        };

        if self.recorder.read(cx).state() != RecordingState::Idle {
            warn!("Audio recording is already active");
            return;
        }

        // Bind the note to the chat it started in *and* to the reply it is
        // answering: resolving either at stop time would misdeliver if the
        // user switches chats meanwhile — which also cancels the draft, so
        // the note arrived in the right conversation with its quote gone.
        let target = RecordingTarget {
            jid,
            reply: self.reply_to.clone(),
        };
        let composer = self.input_area.clone();
        let app = cx.entity().downgrade();
        match self
            .recorder
            .update(cx, |recorder, cx| recorder.open(target, composer, app, cx))
        {
            // No notify: what the microphone being open changes on screen is
            // the composer's panel, and the composer was told directly.
            Ok(()) => info!("PTT recording started"),
            Err(reason) => self.notify_user(reason, crate::app::notices::Tone::Problem, cx),
        }
    }

    /// Stop recording and send the audio message
    pub fn stop_recording_and_send(&mut self, cx: &mut Context<Self>) {
        // Check if connected before attempting to send
        if !self.is_connected() {
            warn!("Cannot send audio: not connected");
            // Said before it is thrown away. This is the path where somebody
            // has already spoken into the microphone, so a recording that
            // vanishes with the reason only in a log is the worst of the
            // three ways this can end.
            self.notify_user(
                "That recording could not be sent: not connected.".to_string(),
                crate::app::notices::Tone::Problem,
                cx,
            );
            self.cancel_recording(cx);
            return;
        }

        if !self.recorder.read(cx).is_recording() {
            warn!("Not recording");
            return;
        }

        let composer = self.input_area.clone();
        let (RecordingTarget { jid, reply }, recording, epoch) = match self
            .recorder
            .update(cx, |recorder, cx| recorder.stop(composer, cx))
        {
            Stopped::Captured {
                target,
                recording,
                epoch,
            } => (target, recording, epoch),
            Stopped::Nowhere => return,
            Stopped::Refused(reason) => {
                self.notify_user(reason, crate::app::notices::Tone::Problem, cx);
                return;
            }
        };

        if self.client.is_none() {
            warn!("Cannot send audio: not connected to the daemon");
            self.notify_user(
                "That recording could not be sent: not connected.".to_string(),
                crate::app::notices::Tone::Problem,
                cx,
            );
            self.settle_recording(cx);
            return;
        }
        // Which recording this encode belongs to. The task below is detached
        // and cannot be stopped from outside, so the only way a teardown can
        // disown it is for it to ask on the way back whether the recording it
        // was started for is still the current one.
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let encoded = Self::finish(cx, recording).await;
            let _ = entity.update(cx, |app, cx| {
                // A disconnect, a logout or a plain cancel while the encoder
                // ran makes this note something nobody is waiting for. Sending
                // it anyway delivers a cancelled recording; and setting `Idle`
                // over a recording that has since started hides its controls
                // with the microphone still open.
                if app.recorder.read(cx).epoch() != epoch {
                    info!("Discarding an encode from a recording that was cancelled");
                    return;
                }
                app.finish_recording_send(jid, reply, encoded, cx);
            });
        })
        .detach();
    }

    /// Turn a stopped recording into the note that gets sent.
    ///
    /// Preparing it — the resample to 16 kHz and the envelope — is the same
    /// pure Rust on both platforms and the expensive half, so on both it goes
    /// to the background executor. On a page that is a `wasm_thread` worker
    /// where the browser gives `gpui_web` the shared memory to make one, and
    /// a `setTimeout(0)` back onto the loop where it does not — a yield
    /// rather than a worker, which is still not the window's current frame.
    /// What the two arms disagree about is only where the *codec* lives: a
    /// desktop's is libopus and follows the preparation onto the same worker,
    /// while a browser's is an `AudioEncoder` that cannot leave the window,
    /// so the prepared note goes back to it and the encoded one is awaited
    /// here.
    ///
    /// The minimum-duration guard stays at the end rather than moving up in
    /// front of the encode, though both arms now know the length before they
    /// start. A recording the browser refused arrives here as a capture of no
    /// length *and* a reason, and the reason is the one worth drawing — so
    /// asking about the length first would answer "too short" to a microphone
    /// that was denied. What it would have saved is the encode of a note
    /// under a second long.
    async fn finish(
        cx: &mut gpui::AsyncApp,
        recording: oxidezap_audio::Recording,
    ) -> Result<(Vec<u8>, Vec<u8>, u32), String> {
        use gpui::AppContext as _;

        let (bytes, waveform, duration_secs) = match recording {
            oxidezap_audio::Recording::Samples(captured) => {
                cx.background_spawn(async move {
                    let prepared = captured.prepare();
                    encode_to_opus_ogg(&prepared)
                        .map(|ogg| (ogg, prepared.waveform, prepared.duration_secs))
                        .map_err(|error| error.to_string())
                })
                .await?
            }
            oxidezap_audio::Recording::Pending {
                captured,
                prepared,
                note,
            } => {
                let ready = cx.background_spawn(async move { captured.prepare() }).await;
                // The recorder's task is gone only if the page is; nothing
                // else drops that receiver. Said rather than swallowed,
                // because the person watched themselves record this.
                if prepared.send(ready).is_err() {
                    return Err("the recording ended before it was encoded".to_string());
                }
                let note = note
                    .await
                    .map_err(|_| "the recording ended before it was encoded".to_string())?
                    .map_err(|e| e.to_string())?;
                (note.bytes, note.waveform, note.duration_secs)
            }
        };

        if duration_secs < 1 {
            return Err("that recording was too short to send".to_string());
        }
        Ok((bytes, waveform, duration_secs))
    }

    fn finish_recording_send(
        &mut self,
        jid: String,
        reply: Option<ReplyDraft>,
        encoded: Result<(Vec<u8>, Vec<u8>, u32), String>,
        cx: &mut Context<Self>,
    ) {
        let (ogg_data, waveform, duration_secs) = match encoded {
            Ok(encoded) => encoded,
            Err(error) => {
                error!("Failed to encode audio: {error}");
                // The person watched themselves record this, so its
                // disappearance needs a sentence. Every message reaching here
                // is written for a reader: a refused microphone, a recording
                // too short to send, an encoder that stopped.
                self.notify_user(error, crate::app::notices::Tone::Problem, cx);
                self.settle_recording(cx);
                return;
            }
        };

        if self.client.is_none() {
            warn!("Cannot send audio: client is unavailable");
            self.notify_user(
                "That recording could not be sent: not connected.".to_string(),
                crate::app::notices::Tone::Problem,
                cx,
            );
            self.settle_recording(cx);
            return;
        }
        // Recording is a way of answering, so the draft the note was bound to
        // belongs to *this* send — and leaving it armed made it attach itself
        // to whatever was typed next, which is the half of the bug nobody
        // would connect to having pressed the microphone. Cleared only if it
        // is still that draft: one picked while the note was being recorded
        // or encoded is answering something else.
        let quoted = reply.map(|draft| {
            if self
                .reply_to
                .as_ref()
                .is_some_and(|current| current.message_id == draft.message_id)
            {
                self.reply_to = None;
                if let Some(input) = &self.input_area {
                    input.update(cx, |view, cx| view.set_reply(None, cx));
                }
            }
            QuotedMessage::from(draft)
        });
        self.send_voice_note(cx, &jid, ogg_data, waveform, duration_secs, quoted);
        self.settle_recording(cx);
        info!("PTT audio sent successfully");
        // The bubble the send just drew is the app's, and this is what puts
        // it on screen.
        cx.notify();
    }

    /// Send encoded opus into a chat and draw the bubble for it.
    ///
    /// Shared with the retry path, because a voice note that failed still
    /// holds everything it needs to go again — the encoded bytes, its length
    /// and its waveform — and re-encoding is not something a retry can do.
    pub(super) fn send_voice_note(
        &mut self,
        cx: &mut App,
        jid: &str,
        ogg_data: Vec<u8>,
        waveform: Vec<u8>,
        duration_secs: u32,
        quoted: Option<QuotedMessage>,
    ) {
        let Some(client) = &self.client else {
            warn!("Cannot send audio: client is unavailable");
            return;
        };
        let local_id = Self::next_local_id("local_audio");
        // Shared with the bubble below rather than moved: our own voice note
        // should draw the same shape the recipient sees, not a flat bar.
        let envelope = Arc::new(waveform.clone());
        client.send_audio_message(
            jid,
            ogg_data.clone(),
            duration_secs,
            waveform,
            local_id.clone(),
            quoted.clone(),
        );

        let mut msg = ChatMessage::new_outgoing_with_media(
            local_id,
            String::new(),
            MediaContent::audio(
                Arc::new(ogg_data),
                "audio/ogg; codecs=opus".to_string(),
                Some(duration_secs),
                Some(envelope),
            ),
        );

        // The bubble shows the quote too, or the sender sees a bare note
        // where the recipient sees a reply.
        msg.quoted = quoted;

        // Following the note down is only what the sender expects if they are
        // looking at where it landed. There is one timeline, so a note that
        // finished encoding after the user moved on would otherwise yank
        // whatever is on screen to its newest message — out from under someone
        // reading its history. Against `visible_chat` and not the selection,
        // because the selection is deliberately *kept* while the reader is in
        // Status or, on a phone, walking the chat list: coming back to a
        // conversation should land where they left it.
        if self.add_message_to_chat(jid, msg, cx) && self.visible_chat.as_deref() == Some(jid) {
            self.scroll_to_last_message();
        }
    }

    /// Cancel recording without sending
    pub fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        let composer = self.input_area.clone();
        self.recorder
            .update(cx, |recorder, cx| recorder.cancel(composer, cx));
        info!("PTT recording cancelled");
    }

    /// Back to idle, without disowning anything: the encode this settles is
    /// the one that just ended.
    fn settle_recording(&mut self, cx: &mut Context<Self>) {
        let composer = self.input_area.clone();
        self.recorder
            .update(cx, |recorder, cx| recorder.settle(composer, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`Recorder`] with no window behind it. The device is not opened by
    /// any of this: what the tests below drive is the state machine that
    /// decides whether an encode is still wanted.
    fn recorder() -> Recorder {
        Recorder::new()
    }

    /// The three states mean three different things to the composer, and
    /// only one of them is the microphone being open.
    #[test]
    fn only_recording_holds_the_microphone() {
        let mut recorder = recorder();
        assert_eq!(recorder.state(), RecordingState::Idle);
        assert!(!recorder.is_recording());

        recorder.state = RecordingState::Recording;
        assert!(recorder.is_recording());

        // Processing is the encode: the microphone is shut, and the panel it
        // was drawing has to go with it.
        recorder.state = RecordingState::Processing;
        assert!(!recorder.is_recording());
    }

    /// Cancelling disowns the encode that is still running, because nothing
    /// can stop it: the completion asks on the way back whether the recording
    /// it was started for is still the current one, and this is the answer
    /// that says no.
    #[test]
    fn cancelling_disowns_an_encode_in_flight() {
        let mut recorder = recorder();
        recorder.state = RecordingState::Processing;
        let encoding = recorder.epoch();

        recorder.disown();

        assert_ne!(
            recorder.epoch(),
            encoding,
            "the note that comes back is not this one's"
        );
        assert_eq!(recorder.state(), RecordingState::Idle);
        assert!(recorder.take_target().is_none(), "and it is bound nowhere");
    }

    /// Settling is what a completed encode does, and it must *not* disown
    /// anything: the recording it settles is the one that just finished, and
    /// bumping here would have every successful send discard the next one.
    #[test]
    fn settling_leaves_the_epoch_where_it_is() {
        let mut recorder = recorder();
        recorder.state = RecordingState::Processing;
        let encoding = recorder.epoch();

        recorder.idle();

        assert_eq!(recorder.epoch(), encoding);
        assert_eq!(recorder.state(), RecordingState::Idle);
    }

    /// The note is bound to where it is going when the microphone opens, not
    /// when it closes: the reader can switch chats or answer something else
    /// while it runs, and both would otherwise be resolved against whatever
    /// the window looks like at the end.
    #[test]
    fn stopping_takes_the_target_the_recording_started_with() {
        let mut recorder = recorder();
        recorder.state = RecordingState::Recording;
        recorder.target = Some(RecordingTarget {
            jid: "111@s.whatsapp.net".to_string(),
            reply: None,
        });

        let bound = recorder.take_target().expect("bound when it opened");

        assert_eq!(bound.jid, "111@s.whatsapp.net");
        assert!(
            recorder.take_target().is_none(),
            "and taken, so a second stop has nowhere to send"
        );
    }
}
