//! Playback and download of message media.
//!
//! Audio, video and image share one owner because they compete for the same
//! output device and the same "what is playing right now" slot.

use super::*;

impl WhatsAppApp {
    /// Update a message's media data (used to cache downloaded media)
    fn update_message_media_data(&mut self, message_id: &str, data: Vec<u8>) {
        // Find the message in any chat and update its media data
        for chat in &mut self.chats {
            if let Some(msg) = chat.messages.iter_mut().find(|m| m.id == message_id) {
                if let Some(ref mut media) = msg.media {
                    media.data = Arc::new(data);
                    // Full bytes landed; the data no longer needs a re-download
                    media.data_is_preview = false;
                    // Decode with the real media's MIME, not the preview
                    // thumbnail's (a WebP sticker would fail as image/jpeg)
                    if let Some(ref dl) = media.downloadable {
                        media.mime_type = dl.mime_type.clone();
                    }
                    // Drop any render-cached image built from the old bytes
                    self.decoded_images.borrow_mut().shift_remove(message_id);
                    info!("Cached media data for message {}", message_id);
                    // Invalidate message cache since we modified the message
                    self.message_list_cache.borrow_mut().remove(&chat.jid);
                }
                return;
            }
        }
    }
    /// Stop any currently playing media. Does NOT call cx.notify().
    pub(super) fn stop_current_media(&mut self) {
        self.audio_player.stop();
        self.audio_owner = None;
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
        self.audio_owner.as_deref()
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
    pub fn seek_audio(&mut self, fraction: f32, cx: &mut Context<Self>) {
        if self.audio_owner.is_none() {
            return;
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
    pub fn cycle_playback_speed(&mut self, cx: &mut Context<Self>) {
        use crate::components::message_bubble::SPEEDS;
        let next = SPEEDS
            .iter()
            .position(|s| (s - self.playback_speed).abs() < f32::EPSILON)
            .map_or(0, |ix| (ix + 1) % SPEEDS.len());
        self.playback_speed = SPEEDS[next];
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
                smol::Timer::after(std::time::Duration::from_millis(66)).await;
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

        let completion_rx = self.audio_player.on_complete();

        match self.audio_player.play(audio_data) {
            Ok(()) => {
                self.audio_owner = Some(message_id.clone());
                self.active_media = ActiveMedia::Audio {
                    message_id: message_id.clone(),
                };
                info!("Started audio playback for message {}", message_id);
                // Drives the playhead and the clock while it runs.
                self.ensure_playback_tick(cx);

                // Wait for completion event (no polling needed)
                let completed_id = message_id;
                cx.spawn(async move |entity: WeakEntity<Self>, cx| {
                    let _ = completion_rx.await;

                    let _ = entity.update(cx, |app, cx| {
                        // Id check, not just is_audio: switching A -> B drops
                        // A's completion sender after B is active, and A's
                        // stale wakeup must not clear B's state.
                        if app.active_media.is_playing(&completed_id) {
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
            }
            cx.notify();
            return;
        }

        let Some(client) = &self.client else {
            warn!("Cannot download audio: client is unavailable");
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
        let Some(client) = &self.client else {
            warn!("Cannot download image: client is unavailable");
            return;
        };
        let download_rx = client.download_downloadable_media(downloadable);

        cx.spawn(async move |entity: WeakEntity<Self>, cx| {
            match download_with_timeout(download_rx).await {
                Ok(data) => {
                    info!("Image downloaded: {} bytes", data.len());
                    let _ = entity.update(cx, |app, cx| {
                        app.update_message_media_data(&message_id, data);
                        cx.notify();
                    });
                }
                Err(e) => {
                    error!("Failed to download image: {}", e);
                }
            }
        })
        .detach();
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
        let Some(client) = &self.client else {
            warn!("Cannot download document: client is unavailable");
            return;
        };
        let download_rx = client.download_downloadable_media(downloadable);
        let runtime = client.runtime();

        cx.spawn(async move |_entity: WeakEntity<Self>, _cx| {
            match download_with_timeout(download_rx).await {
                Ok(data) => {
                    let saved = runtime
                        .spawn_blocking(move || save_to_downloads(&file_name, &data))
                        .await
                        .map_err(|error| std::io::Error::other(error.to_string()))
                        .and_then(|result| result);
                    match saved {
                        Ok(_) => info!("Document {} saved", message_id),
                        Err(e) => warn!("Failed to save document {}: {}", message_id, e),
                    }
                }
                Err(e) => error!("Failed to download document {}: {}", message_id, e),
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
    pub fn get_decoded_image(&self, message_id: &str, data: &[u8], mime_type: &str) -> Arc<Image> {
        // Check if already cached
        if let Some(cached) = self.decoded_images.borrow().get(message_id).cloned() {
            return cached;
        }

        // Create and cache the image
        let format = mime_to_image_format(mime_type);
        let image = Arc::new(Image::from_bytes(format, data.to_vec()));

        let mut cache = self.decoded_images.borrow_mut();

        // Evict oldest entries if cache is full (FIFO eviction using IndexMap insertion order)
        while cache.len() >= MAX_DECODED_IMAGES {
            // shift_remove removes from the front (oldest entry)
            cache.shift_remove_index(0);
        }

        cache.insert(message_id.to_string(), image.clone());
        image
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
                let owns_audio = self.audio_owner.as_deref() == Some(message_id.as_str());
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
                        self.audio_owner = Some(message_id.clone());
                    }
                } else if !needs_audio && self.audio_owner.as_ref() == Some(&message_id) {
                    // Only resume if audio belongs to this video
                    self.audio_player.resume();
                }
            }
            Some(VideoPlayerState::Idle) | Some(VideoPlayerState::Error) => {
                // Start downloading (or retry on error)
                self.start_video_download(message_id, downloadable, cx);
            }
            Some(VideoPlayerState::Downloading) | Some(VideoPlayerState::Decoding) => {
                // Already in progress, do nothing
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
        let runtime = client.runtime();

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

                    let decode_result = match runtime
                        .spawn_blocking(move || StreamingVideoDecoder::new(&data))
                        .await
                    {
                        Ok(result) => result,
                        Err(error) => Err(anyhow::anyhow!("video decoder task failed: {error}")),
                    };

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
                                if let Some(ref jid) = app.selected_chat {
                                    app.invalidate_message_cache(jid);
                                }

                                // Schedule play() for the next frame to allow GPUI to decode the image
                                let msg_id_for_play = msg_id.clone();
                                let audio_for_play = audio;
                                cx.spawn(async move |entity: WeakEntity<Self>, cx| {
                                    // Wait one frame (~16ms at 60fps) for GPUI to decode the first frame
                                    smol::Timer::after(std::time::Duration::from_millis(16)).await;

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
                                                    app.audio_owner = Some(msg_id_for_play.clone());
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
