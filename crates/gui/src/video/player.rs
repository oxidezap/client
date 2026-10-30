//! Video player state management

use std::sync::Arc;
use std::time::Duration;

use gpui::RenderImage;
use tokio::sync::oneshot;
use wacore::time::Instant;

use super::audio::VideoAudio;
use super::platform::StreamingVideoDecoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPlayerState {
    Idle,
    Downloading,
    Decoding,
    Playing,
    Paused,
    Error,
}

impl VideoPlayerState {
    pub fn is_playing(self) -> bool {
        self == Self::Playing
    }

    pub fn is_paused(self) -> bool {
        self == Self::Paused
    }

    pub fn is_loading(self) -> bool {
        matches!(self, Self::Downloading | Self::Decoding)
    }

    pub fn is_error(self) -> bool {
        self == Self::Error
    }
}

/// Video player using StreamingVideoDecoder for memory-efficient on-demand frame decoding.
pub struct VideoPlayer {
    state: VideoPlayerState,
    decoder: Option<StreamingVideoDecoder>,
    playback_start: Option<Instant>,
    paused_at: Option<Duration>,
    error: Option<String>,
    current_frame: Option<Arc<RenderImage>>,
    current_timestamp: Duration,
    audio: Option<Arc<VideoAudio>>,
    needs_audio_start: bool,
    completion_tx: Option<oneshot::Sender<()>>,
    preparation: Arc<()>,
}

impl Default for VideoPlayer {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoPlayer {
    pub fn new() -> Self {
        Self {
            state: VideoPlayerState::Idle,
            decoder: None,
            playback_start: None,
            paused_at: None,
            error: None,
            current_frame: None,
            current_timestamp: Duration::ZERO,
            audio: None,
            needs_audio_start: false,
            completion_tx: None,
            preparation: Arc::new(()),
        }
    }

    /// Subscribe to playback completion (only one subscriber supported at a time).
    pub fn on_complete(&mut self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.completion_tx = Some(tx);
        rx
    }

    pub fn state(&self) -> VideoPlayerState {
        self.state
    }

    pub fn set_downloading(&mut self) -> Arc<()> {
        self.preparation = Arc::new(());
        self.state = VideoPlayerState::Downloading;
        self.error = None;
        Arc::clone(&self.preparation)
    }

    pub fn owns_preparation(&self, preparation: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.preparation, preparation)
    }

    pub fn set_decoding(&mut self) {
        self.state = VideoPlayerState::Decoding;
    }

    pub fn load(&mut self, mut decoder: StreamingVideoDecoder) {
        log::info!(
            "VideoPlayer::load - frames: {}, duration: {:?}",
            decoder.frame_count(),
            decoder.duration()
        );
        decoder.seek_to_frame(0);
        self.decoder = Some(decoder);
        self.state = VideoPlayerState::Paused;
        self.paused_at = Some(Duration::ZERO);
        let frame_updated = self.update_current_frame();
        log::info!(
            "VideoPlayer::load - frame_updated: {}, current_frame.is_some: {}",
            frame_updated,
            self.current_frame.is_some()
        );
    }

    pub fn set_error(&mut self, error: String) {
        self.state = VideoPlayerState::Error;
        self.error = Some(error);
    }

    /// Start or resume playback. Returns true if audio needs to be started.
    pub fn play(&mut self) -> bool {
        let Some(_) = self.decoder.as_ref() else {
            return false;
        };

        let offset = self.paused_at.unwrap_or(Duration::ZERO);
        self.playback_start = Some(Instant::now() - offset);
        self.paused_at = None;
        self.state = VideoPlayerState::Playing;

        if self.needs_audio_start {
            self.needs_audio_start = false;
            return self.audio.is_some();
        }
        false
    }

    pub fn set_audio(&mut self, audio: VideoAudio) {
        self.audio = Some(Arc::new(audio));
        self.needs_audio_start = true;
    }

    pub fn get_audio(&self) -> Option<&VideoAudio> {
        self.audio.as_deref()
    }

    pub fn pause(&mut self) {
        if self.state == VideoPlayerState::Playing {
            self.paused_at = Some(self.current_time());
            self.playback_start = None;
            self.state = VideoPlayerState::Paused;
        }
    }

    pub fn stop(&mut self) {
        self.playback_start = None;
        self.paused_at = Some(Duration::ZERO);
        self.state = VideoPlayerState::Paused;
        if let Some(decoder) = &mut self.decoder {
            decoder.reset();
        }
        self.update_current_frame();
        self.needs_audio_start = true;
        self.completion_tx = None;
    }

    /// Stop a hidden viewer and release temporary decode resources.
    /// Invalidate work from this opening, even if the same message reopens.
    pub fn stop_hidden(&mut self) {
        self.preparation = Arc::new(());
        if self.decoder.is_some() {
            self.stop();
            self.release_decoder();
        } else {
            self.state = VideoPlayerState::Idle;
            self.completion_tx = None;
        }
    }

    /// Give up the codec session while this player is not the one playing.
    ///
    /// Separate from `stop`, which is where a poster frame is asked for and
    /// so is exactly where the decoder is still needed. This is for a player
    /// the cache is keeping around: what cost a download stays, and the
    /// session a browser allows only a few of goes back.
    pub fn release_decoder(&mut self) {
        if let Some(decoder) = &mut self.decoder {
            decoder.release();
        }
    }

    pub fn current_time(&self) -> Duration {
        self.playback_start
            .map(|start| start.elapsed())
            .or(self.paused_at)
            .unwrap_or(Duration::ZERO)
    }

    /// Update the current frame based on playback time. Returns true if frame changed.
    pub fn update(&mut self) -> bool {
        let Some(decoder) = &self.decoder else {
            return false;
        };
        if self.state != VideoPlayerState::Playing {
            // A picture asked for is not a picture in hand. On a push decoder
            // `seek` and `reset` return having *asked*, and the answer lands
            // on a later poll, so a stop that only polled once went on
            // showing the last frame where the desktop, whose decode is
            // inline, shows the first.
            return self.update_current_frame();
        }

        let current_time = self.current_time();

        if current_time >= decoder.duration() {
            // stop() clears completion_tx, so take it first or the notification is lost
            let completion_tx = self.completion_tx.take();
            self.stop();
            if let Some(tx) = completion_tx {
                let _ = tx.send(());
            }
            return true;
        }

        if let Some(decoder) = &mut self.decoder {
            decoder.seek(current_time);
        }
        self.update_current_frame()
    }

    fn update_current_frame(&mut self) -> bool {
        // Asked every poll, because on a push decoder the failure arrives
        // long after the call that caused it: a chunk refused, the error
        // callback firing, a copy that would not complete. Without this the
        // player stays in `Playing` with no picture and nothing to say, which
        // is the one outcome worse than an error.
        if let Some(reason) = self.decoder.as_ref().and_then(|d| d.failure()) {
            self.set_error(reason);
            return false;
        }
        let Some(decoder) = &self.decoder else {
            return false;
        };
        let Some(frame) = decoder.current_frame() else {
            return false;
        };

        let changed = self.current_timestamp != frame.timestamp;
        if changed {
            log::debug!(
                "VideoPlayer: frame {} -> {} ({})",
                self.current_timestamp.as_millis(),
                frame.timestamp.as_millis(),
                frame.index
            );
        }
        self.current_frame = Some(Arc::clone(&frame.image));
        self.current_timestamp = frame.timestamp;
        changed
    }

    pub fn current_frame(&self) -> Option<Arc<RenderImage>> {
        self.current_frame.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_preparation_cannot_replace_a_reopened_player() {
        let mut player = VideoPlayer::new();
        let old = player.set_downloading();
        player.set_decoding();
        player.stop_hidden();
        assert_eq!(player.state(), VideoPlayerState::Idle);
        assert!(!player.owns_preparation(&old));
        let reopened = player.set_downloading();
        assert!(player.owns_preparation(&reopened));
        assert!(!player.owns_preparation(&old));
        let mut replacement = VideoPlayer::new();
        replacement.set_downloading();
        assert!(!replacement.owns_preparation(&reopened));
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn hidden_player_keeps_poster_and_replays_with_shared_audio() {
        let data = super::super::platform::tests::synthetic_video();
        let decoder = StreamingVideoDecoder::new(&data).unwrap();
        let mut player = VideoPlayer::new();
        player.load(decoder);
        let samples: Arc<[f32]> = vec![0.25; 48000].into();
        player.set_audio(VideoAudio {
            samples: samples.clone(),
            sample_rate: 48000,
        });
        let shared_frame = player.current_frame().unwrap();
        let frame = Arc::downgrade(&shared_frame);
        let completion = player.on_complete();
        player.play();
        player.stop_hidden();
        assert!(
            frame.upgrade().is_some(),
            "another visible owner keeps its frame"
        );
        drop(shared_frame);
        assert!(player.current_frame().is_some());
        assert!(completion.blocking_recv().is_err());
        assert!(player.current_frame().is_some());
        assert_eq!(player.current_time(), Duration::ZERO);
        assert!(Arc::ptr_eq(&player.get_audio().unwrap().samples, &samples));
        assert!(player.play());
        player.update();
        assert!(player.current_frame().is_some());
    }
}
