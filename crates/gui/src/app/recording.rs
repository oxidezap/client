//! Voice-message recording: capture, encode and send.

use super::*;

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

        // Initialize and start recording
        if let Err(e) = self.audio_recorder.init() {
            error!("Failed to initialize audio recorder: {}", e);
            return;
        }

        if let Err(e) = self.audio_recorder.start() {
            error!("Failed to start recording: {}", e);
            return;
        }

        self.recording_state = RecordingState::Recording;
        // Bind the note to the chat it started in: resolving the destination
        // at stop time would misdeliver if the user switches chats meanwhile.
        self.recording_chat = self.selected_chat.clone();
        self.update_input_recording(cx);
        info!("PTT recording started");
        cx.notify();
    }
    /// Stop recording and send the audio message
    pub fn stop_recording_and_send(&mut self, cx: &mut Context<Self>) {
        // Check if connected before attempting to send
        if !self.is_connected() {
            warn!("Cannot send audio: not connected");
            self.cancel_recording(cx);
            return;
        }

        if !self.is_recording() {
            warn!("Not recording");
            return;
        }

        let jid = match self.recording_chat.take() {
            Some(jid) => jid,
            None => {
                warn!("No recording chat, cancelling recording");
                self.cancel_recording(cx);
                return;
            }
        };

        self.recording_state = RecordingState::Processing;
        self.update_input_recording(cx);
        cx.notify();

        // Stop recording and get audio data
        let recorded = match self.audio_recorder.stop() {
            Ok(audio) => audio,
            Err(e) => {
                error!("Failed to stop recording: {}", e);
                self.recording_state = RecordingState::Idle;
                // Every abort path must reset the input area too, or it keeps
                // rendering the recording UI forever.
                self.update_input_recording(cx);
                cx.notify();
                return;
            }
        };

        // Check minimum duration (1 second)
        if recorded.duration_secs < 1 {
            warn!("Recording too short, discarding");
            self.recording_state = RecordingState::Idle;
            self.update_input_recording(cx);
            cx.notify();
            return;
        }

        info!(
            "Recording stopped: {} samples, {}s",
            recorded.samples.len(),
            recorded.duration_secs
        );

        let Some(runtime) = self.client.as_ref().map(WhatsAppClient::runtime) else {
            warn!("Cannot send audio: client is unavailable");
            self.recording_state = RecordingState::Idle;
            self.update_input_recording(cx);
            cx.notify();
            return;
        };
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let encoded = runtime
                .spawn_blocking(move || {
                    let waveform = generate_waveform(&recorded.samples);
                    encode_to_opus_ogg(&recorded)
                        .map(|ogg| (ogg, waveform, recorded.duration_secs))
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|error| Err(format!("encoder task failed: {error}")));
            let _ = entity.update(cx, |app, cx| {
                app.finish_recording_send(jid, encoded, cx);
            });
        })
        .detach();
    }
    fn finish_recording_send(
        &mut self,
        jid: String,
        encoded: Result<(Vec<u8>, Vec<u8>, u32), String>,
        cx: &mut Context<Self>,
    ) {
        let (ogg_data, waveform, duration_secs) = match encoded {
            Ok(encoded) => encoded,
            Err(error) => {
                error!("Failed to encode audio: {error}");
                self.recording_state = RecordingState::Idle;
                self.update_input_recording(cx);
                cx.notify();
                return;
            }
        };

        let Some(client) = &self.client else {
            warn!("Cannot send audio: client is unavailable");
            self.recording_state = RecordingState::Idle;
            self.update_input_recording(cx);
            cx.notify();
            return;
        };
        let local_id = Self::next_local_id("local_audio");
        // Shared with the bubble below rather than moved: our own voice note
        // should draw the same shape the recipient sees, not a flat bar.
        let envelope = Arc::new(waveform.clone());
        client.send_audio_message(
            &jid,
            ogg_data.clone(),
            duration_secs,
            waveform,
            local_id.clone(),
        );

        let msg = ChatMessage::new_outgoing_with_media(
            local_id,
            String::new(),
            MediaContent {
                media_type: MediaType::Audio,
                data: Arc::new(ogg_data),
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

        if self.add_message_to_chat(&jid, msg) {
            self.scroll_to_last_message();
        }
        self.recording_state = RecordingState::Idle;
        self.update_input_recording(cx);
        info!("PTT audio sent successfully");
        cx.notify();
    }
    /// Cancel recording without sending
    pub fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        self.audio_recorder.cancel();
        self.recording_state = RecordingState::Idle;
        self.recording_chat = None;
        self.update_input_recording(cx);
        info!("PTT recording cancelled");
        cx.notify();
    }
}
