//! Voice-message recording: capture, encode and send.

use super::*;

/// How often the recording panel is repainted while capture runs.
///
/// Ten a second: fast enough that the meter follows a voice, slow enough that
/// it is nothing next to the audio callback already running.
const RECORDING_TICK_MS: u64 = 100;

impl WhatsAppApp {
    /// Update the recording state in the input area (call only when recording state changes)
    fn update_input_recording(&self, cx: &mut Context<Self>) {
        if let Some(ref input_area) = self.input_area {
            let is_recording = self.is_recording();
            input_area.update(cx, |view, cx| {
                view.set_recording(is_recording, cx);
            });
        }
    }

    /// Repaint the recording panel while capture is running.
    ///
    /// Without it the panel drew once and then sat there: the timer is derived
    /// from an `Instant`, which is only as current as the last repaint, and
    /// the meter had no source at all. This is that source — the recorder's
    /// own buffer, read at a rate a person can follow, and stopped the moment
    /// recording does so an idle window wakes for nothing.
    fn ensure_recording_tick(&mut self, cx: &mut Context<Self>) {
        if self.recording_tick.is_some() || !self.is_recording() {
            return;
        }
        self.recording_tick = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            loop {
                crate::platform::sleep(std::time::Duration::from_millis(RECORDING_TICK_MS)).await;
                let keep_going = entity.update(cx, |app, cx| {
                    if !app.is_recording() {
                        return false;
                    }
                    let level = app.audio_recorder.level();
                    if let Some(ref input_area) = app.input_area {
                        input_area.update(cx, |view, cx| view.set_level(level, cx));
                    }
                    // The clock lives in the view and is read from an
                    // `Instant`, so the repaint above is what advances it.
                    cx.notify();
                    true
                });
                match keep_going {
                    Ok(true) => continue,
                    Ok(false) | Err(_) => break,
                }
            }
            let _ = entity.update(cx, |app, _| app.recording_tick = None);
        }));
    }
    /// Check if currently recording
    pub fn is_recording(&self) -> bool {
        self.recording_state.is_recording()
    }
    /// Start audio recording for PTT
    pub fn start_recording(&mut self, cx: &mut Context<Self>) {
        if self.selected_chat.is_none() {
            warn!("Cannot record: no chat selected");
            return;
        }

        if self.recording_state != RecordingState::Idle {
            warn!("Audio recording is already active");
            return;
        }

        // Refused where nothing can come of it. The composer already draws
        // the microphone disabled there, so this is the keyboard route and
        // anything else that reaches the action directly. Both halves, for
        // the reason the composer asks both: an encoder the browser does not
        // have, and a session with nowhere to send what it encodes, are two
        // ways for the same press to end in a recording that is lost.
        if let Some(reason) = crate::platform::media_send_unavailable() {
            warn!("this session cannot send a voice note: {reason}");
            self.notify_user(reason.to_string(), crate::app::notices::Tone::Problem, cx);
            return;
        }
        if !oxidezap_audio::can_record() {
            warn!("this build cannot record a voice note");
            self.notify_user(
                "Voice messages cannot be recorded in this browser.".to_string(),
                crate::app::notices::Tone::Problem,
                cx,
            );
            return;
        }

        // Initialize and start recording. Said out loud on both paths: a
        // microphone the browser refused, or a device that will not open, is
        // a press that otherwise does nothing at all, the composer stays as
        // it was, with the reason only in the log.
        if let Err(e) = self.audio_recorder.init() {
            error!("Failed to initialize audio recorder: {}", e);
            self.notify_user(
                "The microphone could not be opened.".to_string(),
                crate::app::notices::Tone::Problem,
                cx,
            );
            return;
        }

        if let Err(e) = self.audio_recorder.start() {
            error!("Failed to start recording: {}", e);
            self.notify_user(
                "Recording could not be started.".to_string(),
                crate::app::notices::Tone::Problem,
                cx,
            );
            return;
        }

        self.recording_state = RecordingState::Recording;
        // Bind the note to the chat it started in *and* to the reply it is
        // answering: resolving either at stop time would misdeliver if the
        // user switches chats meanwhile — which also cancels the draft, so
        // the note arrived in the right conversation with its quote gone.
        self.recording_target = self.selected_chat.clone().map(|jid| RecordingTarget {
            jid,
            reply: self.reply_to.clone(),
        });
        self.update_input_recording(cx);
        self.ensure_recording_tick(cx);
        info!("PTT recording started");
        cx.notify();
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

        if !self.is_recording() {
            warn!("Not recording");
            return;
        }

        let Some(RecordingTarget { jid, reply }) = self.recording_target.take() else {
            warn!("No recording chat, cancelling recording");
            self.cancel_recording(cx);
            return;
        };

        self.recording_state = RecordingState::Processing;
        self.update_input_recording(cx);
        cx.notify();

        // Stop recording and get audio data
        let recording = match self.audio_recorder.stop() {
            Ok(recording) => recording,
            Err(e) => {
                error!("Failed to stop recording: {}", e);
                self.notify_user(
                    "The recording could not be stopped.".to_string(),
                    crate::app::notices::Tone::Problem,
                    cx,
                );
                self.recording_state = RecordingState::Idle;
                // Every abort path must reset the input area too, or it keeps
                // rendering the recording UI forever.
                self.update_input_recording(cx);
                cx.notify();
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
            self.recording_state = RecordingState::Idle;
            self.update_input_recording(cx);
            cx.notify();
            return;
        }
        // Which recording this encode belongs to. The task below is detached
        // and cannot be stopped from outside, so the only way a teardown can
        // disown it is for it to ask on the way back whether the recording it
        // was started for is still the current one.
        let epoch = self.recording_epoch;
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let encoded = Self::finish(cx, recording).await;
            let _ = entity.update(cx, |app, cx| {
                // A disconnect, a logout or a plain cancel while the encoder
                // ran makes this note something nobody is waiting for. Sending
                // it anyway delivers a cancelled recording; and setting `Idle`
                // over a recording that has since started hides its controls
                // with the microphone still open.
                if app.recording_epoch != epoch {
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
    /// The two arms are the two platforms, and the difference is *when* the
    /// encode happens rather than what it produces. A desktop hands back samples
    /// and this encodes them on the background pool, which is real work and
    /// belongs off the UI thread. A browser encoded as it captured, so there is
    /// nothing to do but wait for the last packets to flush.
    ///
    /// The minimum-duration guard lives here rather than before the stop, because
    /// only one of the two knows how long the recording was without waiting for
    /// it.
    async fn finish(
        cx: &mut gpui::AsyncApp,
        recording: oxidezap_audio::Recording,
    ) -> Result<(Vec<u8>, Vec<u8>, u32), String> {
        use gpui::AppContext as _;

        let (bytes, waveform, duration_secs) = match recording {
            oxidezap_audio::Recording::Samples(recorded) => {
                cx.background_spawn(async move {
                    let waveform = generate_waveform(&recorded.samples);
                    encode_to_opus_ogg(&recorded)
                        .map(|ogg| (ogg, waveform, recorded.duration_secs))
                        .map_err(|error| error.to_string())
                })
                .await?
            }
            oxidezap_audio::Recording::Pending(pending) => {
                let note = pending
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
                self.recording_state = RecordingState::Idle;
                self.update_input_recording(cx);
                cx.notify();
                return;
            }
        };

        let Some(client) = &self.client else {
            warn!("Cannot send audio: client is unavailable");
            self.notify_user(
                "That recording could not be sent: not connected.".to_string(),
                crate::app::notices::Tone::Problem,
                cx,
            );
            self.recording_state = RecordingState::Idle;
            self.update_input_recording(cx);
            cx.notify();
            return;
        };
        let _ = client;
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
        self.send_voice_note(&jid, ogg_data, waveform, duration_secs, quoted);
        self.recording_state = RecordingState::Idle;
        self.update_input_recording(cx);
        info!("PTT audio sent successfully");
        cx.notify();
    }

    /// Send encoded opus into a chat and draw the bubble for it.
    ///
    /// Shared with the retry path, because a voice note that failed still
    /// holds everything it needs to go again — the encoded bytes, its length
    /// and its waveform — and re-encoding is not something a retry can do.
    pub(super) fn send_voice_note(
        &mut self,
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
            MediaContent {
                media_type: MediaType::Audio,
                data: Arc::new(ogg_data),
                cache_key: None,
                mime_type: "audio/ogg; codecs=opus".to_string(),
                width: None,
                height: None,
                caption: None,
                file_name: None,
                downloadable: None,
                is_animated: false,
                duration_secs: Some(duration_secs),
                data_is_preview: false,
                waveform: Some(envelope),
            },
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
        if self.add_message_to_chat(jid, msg) && self.visible_chat.as_deref() == Some(jid) {
            self.scroll_to_last_message();
        }
    }
    /// Cancel recording without sending
    pub fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        self.audio_recorder.cancel();
        self.recording_state = RecordingState::Idle;
        self.recording_target = None;
        // Disown an encode that is still running. Cancelling is the only way
        // out of `Processing` other than the completion itself, which is why
        // this is the one place that has to say so.
        self.recording_epoch = self.recording_epoch.wrapping_add(1);
        self.update_input_recording(cx);
        info!("PTT recording cancelled");
        cx.notify();
    }
}
