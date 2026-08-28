//! Playback and download of message media.
//!
//! Audio, video and image share one owner because they compete for the same
//! output device and the same "what is playing right now" slot.

use super::*;

impl WhatsAppApp {
    /// Update a message's media data (used to cache downloaded media)
    fn update_message_media_data(&mut self, message_id: &str, data: Vec<u8>) {
        // Find the message in any chat and update its media data
        let mut touched: Option<String> = None;
        for chat in &mut self.chats {
            if let Some(msg) = chat.messages.iter_mut().find(|m| m.id == message_id) {
                if let Some(ref mut media) = msg.media {
                    // Bytes and the metadata that describes them, together:
                    // decoding a WebP sticker as the `image/jpeg` its poster
                    // frame claimed fails every time.
                    media.adopt_full_bytes(Arc::new(data));
                    // Drop any render-cached image built from the old bytes
                    self.decoded_images.borrow_mut().shift_remove(message_id);
                    info!("Cached media data for message {}", message_id);
                    touched = Some(chat.jid.clone());
                }
                break;
            }
        }

        // Through the shared invalidation rather than by poking one cache:
        // the message list is not the only thing derived from these messages,
        // and the status feed — which is — went on serving the version with no
        // bytes in it, so a downloaded update stayed "cannot be shown".
        if let Some(jid) = touched {
            self.invalidate_message_cache(&jid);
        }
    }
    /// Stop any currently playing media. Does NOT call cx.notify().
    pub(super) fn stop_current_media(&mut self) {
        self.audio_player.stop();
        // Name and bytes together, always: this is the whole reason they are
        // one field.
        self.audio = AudioHolder::None;
        // An in-flight lazy download for the stopped media must not autoplay
        // when it completes; user-initiated requests re-set this after the stop.
        self.pending_media_request = None;

        if let ActiveMedia::Video { message_id } = &self.active_media {
            if let Some(player) = self.video_players.get_mut(message_id) {
                player.stop();
            }
            self.video_update_task = None;
        }

        self.active_media = ActiveMedia::None;
        // Whatever was playing is over, so any completion still in flight for
        // it describes a playback that no longer exists. Bumped here rather
        // than where each playback starts, because every start goes through
        // this and "stopped" is exactly when an outstanding completion stops
        // meaning anything.
        self.playback_epoch = self.playback_epoch.wrapping_add(1);
    }
    /// Get the currently playing audio message ID (if audio is playing)
    pub fn playing_message_id(&self) -> Option<&str> {
        match &self.active_media {
            // Gated on the stream so a paused voice note renders as paused;
            // resume still works because toggle_audio matches on active_media.
            ActiveMedia::Audio { message_id } if self.audio_player.is_playing() => Some(message_id),
            _ => None,
        }
    }
    /// Which clip the player currently holds, playing or paused.
    ///
    /// Distinct from [`Self::playing_message_id`], which is gated on the
    /// stream: progress belongs to whichever note is *loaded*, so a paused
    /// bubble keeps its position instead of snapping back to zero.
    pub fn audio_owner(&self) -> Option<&str> {
        self.audio.message_id()
    }

    /// How far through the loaded voice note playback is, in `0.0..=1.0`.
    pub fn audio_progress(&self) -> f32 {
        self.audio_player.progress()
    }

    /// Seconds played of the loaded voice note.
    pub fn audio_elapsed_secs(&self) -> f32 {
        self.audio_player.elapsed_secs()
    }

    /// Jump to `fraction` of the way through the loaded voice note.
    ///
    /// Playback continues from there rather than restarting, which is what
    /// makes the waveform a scrub bar and not a progress read-out.
    pub fn seek_audio(&mut self, message_id: &str, fraction: f32, cx: &mut Context<Self>) {
        // Named, not merely "something is loaded": every downloaded voice note
        // draws a scrubbable waveform, and an unnamed seek let a click on one
        // row move the position of whichever clip happened to be loaded.
        if self.audio.message_id() != Some(message_id) {
            return;
        }
        // A clip that ran to its end still owns an open stream, so rewinding
        // its position alone would replay the audio with the UI insisting
        // nothing is playing and no completion left to fire. Scrubbing a
        // finished note means playing it again from the bytes still held.
        if self.audio_player.is_finished()
            && let Some(bytes) = self.audio.note_source(message_id)
        {
            self.play_audio(message_id.to_string(), (*bytes).clone(), cx);
        }
        self.audio_player.seek(fraction);
        self.ensure_playback_tick(cx);
        cx.notify();
    }

    /// The current playback speed.
    pub fn playback_speed(&self) -> f32 {
        self.playback_speed
    }

    /// Step to the next speed, wrapping at the end.
    ///
    /// The chosen speed outlives the clip: someone who listens at 1.5× means
    /// it for the next note too.
    ///
    /// The samples are re-timed when a clip is prepared, so a change while one
    /// is playing has to prepare it again — from the bytes kept for exactly
    /// this, and resumed where the listener was rather than from the top.
    pub fn cycle_playback_speed(&mut self, cx: &mut Context<Self>) {
        use crate::components::message_bubble::SPEEDS;
        let next = SPEEDS
            .iter()
            .position(|s| (s - self.playback_speed).abs() < f32::EPSILON)
            .map_or(0, |ix| (ix + 1) % SPEEDS.len());
        self.playback_speed = SPEEDS[next];
        self.audio_player.set_speed(self.playback_speed);

        // Whether it is *playing* is not the question: `set_speed` only takes
        // effect the next time a clip is prepared, so a paused note resumed
        // afterwards ran at the old rate while the chip and the clock had
        // already moved to the new one. What matters is whether a clip is
        // loaded — and one that was paused is put back paused.
        if let Some((message_id, bytes)) =
            self.audio
                .message_id()
                .map(str::to_owned)
                .and_then(|message_id| {
                    let source = self.audio.note_source(&message_id)?;
                    Some((message_id, source))
                })
        {
            let at = self.audio_player.progress();
            let was_playing = self.audio_player.is_playing();
            self.play_audio(message_id, (*bytes).clone(), cx);
            self.audio_player.seek(at);
            if !was_playing {
                self.audio_player.pause();
            }
        }
        cx.notify();
    }

    /// Repaint while audio plays, so the playhead and clock advance.
    ///
    /// Position is read from the player rather than counted here, so a late
    /// or dropped frame costs smoothness and never accuracy. Stops as soon as
    /// nothing is playing.
    pub(super) fn ensure_playback_tick(&mut self, cx: &mut Context<Self>) {
        if self.playback_tick.is_some() {
            return;
        }
        self.playback_tick = Some(cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            loop {
                // ~15fps: the playhead only has to look continuous, and a
                // voice note is not worth a frame-rate repaint of the list.
                crate::platform::sleep(std::time::Duration::from_millis(66)).await;
                let playing = entity.update(cx, |app, cx| {
                    let playing = app.audio_player.is_playing();
                    if playing {
                        cx.notify();
                    }
                    playing
                });
                match playing {
                    Ok(true) => continue,
                    Ok(false) | Err(_) => break,
                }
            }
            let _ = entity.update(cx, |app, _| app.playback_tick = None);
        }));
    }

    /// Get the currently playing video message ID (if video is playing)
    pub fn playing_video_id(&self) -> Option<&str> {
        match &self.active_media {
            ActiveMedia::Video { message_id } => Some(message_id),
            _ => None,
        }
    }
    pub fn play_audio(&mut self, message_id: String, audio_data: Vec<u8>, cx: &mut Context<Self>) {
        self.stop_current_media();
        self.pending_media_request = Some(message_id.clone());
        // Whatever the chip says, applied before the clip is prepared: the
        // re-timing happens once, on these samples.
        self.audio_player.set_speed(self.playback_speed);

        let completion_rx = self.audio_player.on_complete();
        let source = Arc::new(audio_data);

        match self.audio_player.play((*source).clone()) {
            Ok(()) => {
                self.audio = AudioHolder::Note {
                    message_id: message_id.clone(),
                    source,
                };
                self.active_media = ActiveMedia::Audio {
                    message_id: message_id.clone(),
                };
                info!("Started audio playback for message {}", message_id);
                // Drives the playhead and the clock while it runs.
                self.ensure_playback_tick(cx);

                // Wait for completion event (no polling needed)
                let completed_id = message_id;
                // Which playback this belongs to. The id alone is not enough:
                // replaying the *same* note — scrubbing one that ran to its
                // end does exactly that — drops the first playback's sender
                // while the id still matches, so the old wakeup would clear
                // the new playback's state with the audio still running.
                let epoch = self.playback_epoch;
                cx.spawn(async move |entity: WeakEntity<Self>, cx| {
                    let _ = completion_rx.await;

                    let _ = entity.update(cx, |app, cx| {
                        // Id check, not just is_audio: switching A -> B drops
                        // A's completion sender after B is active, and A's
                        // stale wakeup must not clear B's state.
                        if app.playback_epoch == epoch && app.active_media.is_playing(&completed_id)
                        {
                            app.active_media = ActiveMedia::None;
                            info!("Audio playback completed");
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            Err(e) => {
                error!("Failed to play audio: {}", e);
            }
        }
        cx.notify();
    }
    /// Stop audio playback (only if audio is currently playing)
    pub fn stop_audio(&mut self, cx: &mut Context<Self>) {
        if self.active_media.is_audio() {
            self.audio_player.stop();
            // The clip goes with the stream. Left behind, a speed change
            // would prepare bytes the sink no longer has anything to do with.
            self.audio = AudioHolder::None;
            self.active_media = ActiveMedia::None;
            cx.notify();
        }
    }
    /// Toggle play/pause for the current audio
    pub fn toggle_audio(
        &mut self,
        message_id: String,
        audio_data: Vec<u8>,
        cx: &mut Context<Self>,
    ) {
        if self.active_media.is_playing(&message_id) && self.active_media.is_audio() {
            // Same message - toggle play/pause
            if self.audio_player.is_playing() {
                self.audio_player.pause();
            } else {
                self.audio_player.resume();
                // The tick loop ends as soon as playback stops, so a resume
                // has to start a new one or the playhead never moves again.
                self.ensure_playback_tick(cx);
            }
        } else {
            // Different message or not playing - play it
            self.play_audio(message_id, audio_data, cx);
        }
        cx.notify();
    }
    /// Toggle audio playback with lazy loading (download first if needed)
    pub fn toggle_audio_lazy(
        &mut self,
        message_id: String,
        downloadable: DownloadableMedia,
        cx: &mut Context<Self>,
    ) {
        // If already playing this audio message, just toggle
        if self.active_media.is_playing(&message_id) && self.active_media.is_audio() {
            if self.audio_player.is_playing() {
                self.audio_player.pause();
            } else {
                self.audio_player.resume();
                self.ensure_playback_tick(cx);
            }
            cx.notify();
            return;
        }

        // A second tap while the first is still in flight downloads the note
        // twice, and both answers still match `pending_media_request` — so the
        // later one calls `play_audio` again and restarts the note from the
        // top under the listener. The image and document paths already claim
        // this slot; the one that autoplays needed it most.
        if !self.begin_download(&message_id, cx) {
            return;
        }

        let Some(client) = &self.client else {
            warn!("Cannot download audio: client is unavailable");
            self.finish_download(&message_id);
            return;
        };
        let download_rx = client.download_downloadable_media(downloadable);

        // Stop any current playback, video included: a playing video must not
        // keep running underneath the download.
        self.stop_current_media();
        self.pending_media_request = Some(message_id.clone());

        let msg_id = message_id.clone();

        info!("Starting audio download for message {}", msg_id);

        // Spawn a GPUI task to await the download result with timeout
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            match download_with_timeout(download_rx).await {
                Ok(data) => {
                    info!("Audio downloaded: {} bytes", data.len());

                    // Cache the downloaded audio and play it
                    let _ = entity.update(cx, |app, cx| {
                        app.finish_download(&msg_id);
                        // Cache the audio data in the message so we don't need to download again
                        app.update_message_media_data(&msg_id, data.clone());
                        // Autoplay only if the user hasn't started other media
                        // since this download began.
                        if app.pending_media_request.as_deref() == Some(msg_id.as_str()) {
                            app.play_audio(msg_id, data, cx);
                        } else {
                            cx.notify();
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to download audio: {}", e);
                    let _ = entity.update(cx, |app, cx| {
                        app.finish_download(&msg_id);
                        cx.notify();
                    });
                }
            }
        })
        .detach();

        cx.notify();
    }
    /// Fetch the full image for a bubble whose eager download failed and left
    /// no thumbnail; mirrors the audio lazy-download path (no autoplay, the
    /// cached bytes just render).
    pub fn download_image(
        &mut self,
        message_id: String,
        downloadable: DownloadableMedia,
        cx: &mut Context<Self>,
    ) {
        // A second tap while the first is still in flight would download the
        // same bytes twice; the marker is also what the bubble reads to show
        // that something is happening.
        // The slot is claimed *before* the request goes out. Asking first and
        // checking afterwards meant a double tap issued two daemon downloads
        // and then abandoned the second receiver.
        if !self.begin_download(&message_id, cx) {
            return;
        }
        let download_rx = {
            let Some(client) = &self.client else {
                warn!("Cannot download image: client is unavailable");
                self.finish_download(&message_id);
                return;
            };
            client.download_downloadable_media(downloadable)
        };

        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            let result = download_with_timeout(download_rx).await;
            let _ = entity.update(cx, |app, cx| {
                app.finish_download(&message_id);
                match result {
                    Ok(data) => {
                        info!("Image downloaded: {} bytes", data.len());
                        app.update_message_media_data(&message_id, data);
                    }
                    Err(e) => error!("Failed to download image: {}", e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether a download for this message is in flight.
    pub fn is_downloading(&self, message_id: &str) -> bool {
        self.downloads_in_flight.contains(message_id)
    }

    /// Claim the download slot for a message.
    ///
    /// Returns whether the caller owns it — `false` means one is already
    /// running and this tap should be ignored rather than duplicated.
    fn begin_download(&mut self, message_id: &str, cx: &mut Context<Self>) -> bool {
        if !self.downloads_in_flight.insert(message_id.to_string()) {
            return false;
        }
        cx.notify();
        true
    }

    fn finish_download(&mut self, message_id: &str) {
        self.downloads_in_flight.remove(message_id);
    }
    /// Download a document and save it to the user's Downloads directory.
    /// Documents open in external apps, so bytes on disk beat cached bytes.
    pub fn download_document(
        &mut self,
        message_id: String,
        file_name: String,
        downloadable: DownloadableMedia,
        cx: &mut Context<Self>,
    ) {
        if self.client.is_none() {
            warn!("Cannot download document: client is unavailable");
            return;
        }
        // The same slot an image claims, so the card can say "Saving…" and a
        // second tap does not start a second download.
        if !self.begin_download(&message_id, cx) {
            return;
        }
        let Some(client) = &self.client else {
            self.finish_download(&message_id);
            return;
        };
        let download_rx = client.download_downloadable_media(downloadable);

        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            match download_with_timeout(download_rx).await {
                Ok(data) => match hand_to_user(cx, file_name, data).await {
                    Ok(where_it_went) => info!("Document {message_id} saved to {where_it_went}"),
                    Err(e) => warn!("Failed to save document {message_id}: {e}"),
                },
                Err(e) => error!("Failed to download document {}: {}", message_id, e),
            }
            let _ = entity.update(cx, |app, cx| {
                app.finish_download(&message_id);
                cx.notify();
            });
        })
        .detach();
    }
    /// Save a picture already in hand to the Downloads directory.
    ///
    /// Distinct from `download_document`, which fetches first: by the time
    /// the viewer can show a picture its bytes are local, and re-fetching
    /// them to save them would cost a round trip for nothing.
    pub fn save_media(&mut self, message_id: &str, cx: &mut Context<Self>) {
        let Some(media) = self
            .selected_chat_data()
            .and_then(|chat| {
                chat.messages
                    .iter()
                    .find(|message| message.id == message_id)
                    .cloned()
            })
            .and_then(|message| message.media)
            .filter(|media| !media.data.is_empty())
        else {
            warn!("nothing to save for {message_id}");
            return;
        };

        let file_name = media
            .file_name
            .clone()
            .unwrap_or_else(|| default_media_name(message_id, &media.mime_type));
        let data = Arc::clone(&media.data);
        let id = message_id.to_string();

        cx.spawn(async move |_entity: WeakEntity<Self>, cx| {
            match hand_to_user(cx, file_name, data.to_vec()).await {
                Ok(where_it_went) => info!("Saved {id} to {where_it_went}"),
                Err(e) => warn!("Failed to save {id}: {e}"),
            }
        })
        .detach();
    }

    /// Get the video player state for a message (if any)
    pub fn video_player_state(&self, message_id: &str) -> Option<VideoPlayerState> {
        self.video_players.get(message_id).map(|p| p.state())
    }
    /// Get current video frame for a message (if playing).
    /// Returns an `Arc<RenderImage>` — YUV→RGBA was already converted when
    /// the frame was decoded (same pattern Zed uses on Linux).
    pub fn video_current_frame(&self, message_id: &str) -> Option<Arc<gpui::RenderImage>> {
        self.video_players
            .get(message_id)
            .and_then(|p| p.current_frame())
    }
    /// Get or create the decoded image for a message.
    ///
    /// Two reasons this is cached rather than rebuilt per render: GPUI tracks
    /// animation state per `Arc<Image>`, so a fresh one restarts an animated
    /// sticker on every frame; and building one copies the encoded bytes and
    /// makes GPUI decode them again. `update_message_media_data` evicts the
    /// entry when the real bytes replace a preview, so a stale thumbnail cannot
    /// outlive its download.
    /// Uses interior mutability (RefCell) so it can be called during immutable render.
    ///
    /// `None` where `data` is not a still picture — a video's bytes are its
    /// own file once it has been fetched, and nothing decodes those as one.
    /// Answered before the cache is touched, so an MP4 cannot take a slot
    /// from the pictures the cache exists for.
    pub fn get_decoded_image(
        &self,
        message_id: &str,
        data: &[u8],
        mime_type: &str,
    ) -> Option<Arc<Image>> {
        let format = mime_to_image_format(mime_type)?;

        // Check if already cached
        if let Some(cached) = self.decoded_images.borrow().get(message_id).cloned() {
            return Some(cached);
        }

        let image = Arc::new(Image::from_bytes(format, data.to_vec()));

        let mut cache = self.decoded_images.borrow_mut();

        // Evict oldest entries if cache is full (FIFO eviction using IndexMap insertion order)
        while cache.len() >= MAX_DECODED_IMAGES {
            // shift_remove removes from the front (oldest entry)
            cache.shift_remove_index(0);
        }

        cache.insert(message_id.to_string(), image.clone());
        Some(image)
    }
    /// Toggle video playback for a message
    pub fn toggle_video(
        &mut self,
        message_id: String,
        downloadable: DownloadableMedia,
        cx: &mut Context<Self>,
    ) {
        // Get player state first to determine action
        let player_state = self.video_players.get(&message_id).map(|p| p.state());

        match player_state {
            Some(VideoPlayerState::Playing) => {
                // Pause video and its audio
                if let Some(player) = self.video_players.get_mut(&message_id) {
                    player.pause();
                }
                self.audio_player.pause();
                self.active_media = ActiveMedia::None;
                self.video_update_task = None;
            }
            Some(VideoPlayerState::Paused) => {
                // Pausing cleared active_media, so is_playing alone can't
                // tell "nothing else started" from "another media is live";
                // audio ownership does. Stopping here on resume would drop
                // this video's own paused audio and it would come back mute.
                let owns_audio = self.audio.message_id() == Some(message_id.as_str());
                if !self.active_media.is_playing(&message_id) && !owns_audio {
                    self.stop_current_media();
                }
                self.pending_media_request = Some(message_id.clone());

                let (needs_audio, audio_data) =
                    if let Some(player) = self.video_players.get_mut(&message_id) {
                        let resume_pos = player.current_time();
                        let needs = player.play();
                        let data = if needs {
                            player
                                .get_audio()
                                .map(|a| (a.samples.clone(), a.sample_rate))
                        } else if !owns_audio {
                            // Another media's start stole the paused sink, so a
                            // plain resume() would leave this video silent;
                            // re-feed its audio from the pause position
                            // (samples are mono, so offset is seconds * rate).
                            player.get_audio().map(|a| {
                                let offset = ((resume_pos.as_secs_f64() * a.sample_rate as f64)
                                    as usize)
                                    .min(a.samples.len());
                                (a.samples[offset..].to_vec(), a.sample_rate)
                            })
                        } else {
                            None
                        };
                        (needs, data)
                    } else {
                        return;
                    };

                self.active_media = ActiveMedia::Video {
                    message_id: message_id.clone(),
                };
                self.start_video_update_task(cx);

                if let Some((samples, sample_rate)) = audio_data {
                    info!(
                        "Playing video audio: {} samples at {} Hz",
                        samples.len(),
                        sample_rate
                    );
                    if let Err(e) = self.audio_player.play_samples(samples, sample_rate) {
                        warn!("Failed to play video audio: {}", e);
                    } else {
                        // Ownership only on success: recording it for a dead
                        // sink would turn every later resume into a silent
                        // no-op resume().
                        self.audio = AudioHolder::VideoTrack {
                            message_id: message_id.clone(),
                        };
                    }
                } else if !needs_audio && self.audio.message_id() == Some(message_id.as_str()) {
                    // Only resume if audio belongs to this video
                    self.audio_player.resume();
                }
            }
            Some(VideoPlayerState::Idle) | Some(VideoPlayerState::Error) => {
                // Start downloading (or retry on error)
                self.start_video_download(message_id, downloadable, cx);
            }
            Some(VideoPlayerState::Downloading) | Some(VideoPlayerState::Decoding) => {
                // Already on its way — but say again that it is wanted.
                // Leaving the video clears `pending_media_request`, and the
                // completion autoplays on the strength of that alone, so
                // coming back before it finished used to land on a paused
                // first frame with no play control anywhere in the status
                // reader: stuck until the reader was closed and reopened.
                self.pending_media_request = Some(message_id);
            }
            None => {
                // No player yet, start downloading
                self.start_video_download(message_id, downloadable, cx);
            }
        }
        cx.notify();
    }
    /// Start downloading a video for playback
    fn start_video_download(
        &mut self,
        message_id: String,
        downloadable: DownloadableMedia,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = &self.client else {
            warn!("Cannot download video: client is unavailable");
            return;
        };
        let download_rx = client.download_downloadable_media(downloadable);

        // Stop any currently playing media (mutual exclusion)
        self.stop_current_media();
        self.pending_media_request = Some(message_id.clone());

        // Evict old video players if cache is full (excluding currently playing)
        if self.video_players.len() >= MAX_VIDEO_PLAYERS {
            // Remove players that aren't currently playing, up to half the limit
            let current_playing = self.playing_video_id().map(|s| s.to_string());
            let to_remove: Vec<_> = self
                .video_players
                .keys()
                .filter(|k| Some(*k) != current_playing.as_ref() && **k != message_id)
                .take(MAX_VIDEO_PLAYERS / 2)
                .cloned()
                .collect();
            for key in to_remove {
                self.video_players.remove(&key);
            }
        }

        // Create or get player and set to downloading
        let player = self.video_players.entry(message_id.clone()).or_default();
        player.set_downloading();

        let msg_id = message_id.clone();

        // Spawn a GPUI task to await the download result with timeout
        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            match download_with_timeout(download_rx).await {
                Ok(data) => {
                    info!("Video downloaded: {} bytes", data.len());

                    // Set to decoding state (quick UI update)
                    let _ = entity.update(cx, |app, cx| {
                        if let Some(player) = app.video_players.get_mut(&msg_id) {
                            player.set_decoding();
                        }
                        cx.notify();
                    });

                    let decode_result = cx
                        .background_spawn(async move { StreamingVideoDecoder::new(&data) })
                        .await;

                    // Update UI with decode results
                    let _ = entity.update(cx, |app, cx| {
                        match decode_result {
                            Ok(mut decoder) => {
                                info!(
                                    "Video decoded: {} frames, {:.1}s",
                                    decoder.frame_count(),
                                    decoder.duration().as_secs_f64()
                                );

                                // Extract audio before loading decoder into player
                                let audio = decoder.take_audio();

                                if let Some(player) = app.video_players.get_mut(&msg_id) {
                                    player.load(decoder);

                                    // Store audio in player for replay capability
                                    if let Some(ref audio_data) = audio {
                                        player.set_audio(audio_data.clone());
                                    }
                                    // Don't call play() here - let the first frame render first
                                    // so GPUI can decode the WebP image before playback starts
                                }

                                // Invalidate message cache to force virtual list re-render
                                if let Some(jid) = app.selected_chat.clone() {
                                    app.invalidate_message_cache(&jid);
                                }

                                // Schedule play() for the next frame to allow GPUI to decode the image
                                let msg_id_for_play = msg_id.clone();
                                let audio_for_play = audio;
                                cx.spawn(async move |entity: WeakEntity<Self>, cx| {
                                    // Wait one frame (~16ms at 60fps) for GPUI to decode the first frame
                                    crate::platform::sleep(std::time::Duration::from_millis(16))
                                        .await;

                                    let _ = entity.update(cx, |app, cx| {
                                        // Skip autoplay when the user started
                                        // other media during download/decode.
                                        if app.pending_media_request.as_deref()
                                            == Some(msg_id_for_play.as_str())
                                            && let Some(player) =
                                                app.video_players.get_mut(&msg_id_for_play)
                                            && player.state() == VideoPlayerState::Paused
                                        {
                                            let needs_audio = player.play();
                                            app.active_media = ActiveMedia::Video {
                                                message_id: msg_id_for_play.clone(),
                                            };
                                            app.start_video_update_task(cx);

                                            if needs_audio && let Some(audio) = audio_for_play {
                                                info!(
                                                    "Playing video audio: {} samples at {} Hz",
                                                    audio.samples.len(),
                                                    audio.sample_rate
                                                );
                                                if let Err(e) = app
                                                    .audio_player
                                                    .play_samples(audio.samples, audio.sample_rate)
                                                {
                                                    warn!("Failed to play video audio: {}", e);
                                                } else {
                                                    app.audio = AudioHolder::VideoTrack {
                                                        message_id: msg_id_for_play.clone(),
                                                    };
                                                }
                                            }
                                        }
                                        cx.notify();
                                    });
                                })
                                .detach();
                            }
                            Err(e) => {
                                error!("Failed to decode video: {}", e);
                                if let Some(player) = app.video_players.get_mut(&msg_id) {
                                    player.set_error(e.to_string());
                                }
                            }
                        }
                        cx.notify();
                    });
                }
                Err(e) => {
                    error!("Failed to download video: {}", e);
                    let _ = entity.update(cx, |app, cx| {
                        if let Some(player) = app.video_players.get_mut(&msg_id) {
                            player.set_error(e);
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();

        cx.notify();
    }
}

/// Put a file where the user keeps things, on whichever thread can.
///
/// The desktop write is blocking I/O and belongs off the UI thread. The web
/// one reaches for `document`, which exists on one thread only — and gpui's
/// background executor is a real worker there — so it has to stay. One place
/// asks; the call sites above do not care.
async fn hand_to_user(
    cx: &mut gpui::AsyncApp,
    file_name: String,
    data: Vec<u8>,
) -> Result<String, String> {
    if crate::platform::download::SAVES_OFF_THREAD {
        cx.background_spawn(async move { crate::platform::download::save(&file_name, &data) })
            .await
    } else {
        crate::platform::download::save(&file_name, &data)
    }
}
