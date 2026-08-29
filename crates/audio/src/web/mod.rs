//! The audio backend a page has.
//!
//! Three of the five things this crate does survive the move to the web, and
//! two do not — for one reason in both cases, which is worth stating once:
//! **libopus is C**, and C does not build for `wasm32-unknown-unknown`.
//!
//! What survives:
//!
//! - **Playback**, because the browser decodes Opus itself. `decodeAudioData`
//!   takes exactly the bytes the daemon sends, so [`player`] is a real
//!   implementation rather than a stub — voice notes and video sound play.
//! - **Waveforms**, which are arithmetic over samples and shared with the
//!   desktop unchanged.
//!
//! What does not, and why it is reported rather than faked:
//!
//! - **Recording and encoding.** Capture itself is available to a page —
//!   `cpal` has a WebAudio backend and it compiles — but a voice note is
//!   *Opus in an OGG container*, and there is no Opus encoder in this tree
//!   that a page can run. Capturing samples nothing can encode would give the
//!   user a recording UI that always fails at the end, so the microphone is
//!   refused at the start instead, where the interface can say so.
//!
//!   The browser's own `MediaRecorder` does produce Opus, and is the route
//!   in: it hands back encoded bytes rather than the `RecordedAudio` samples
//!   this crate's API is written around, so taking it is an API change rather
//!   than a backend, and belongs with the front end that stages the payload.
//!
//! - **Call devices.** The process that owns the WhatsApp session owns the
//!   microphone (see AGENTS.md), and on the web that process is the daemon.
//!   A page never opens either end of a call's audio, so these exist to keep
//!   one API across platforms and are never reached.

mod player;

pub use player::AudioPlayer;

use crate::recorder::{RecordedAudio, RecorderError};

/// Records nothing, and says so before anything is drawn.
///
/// Mirrors the native recorder's API exactly, so the front end above it has
/// no `cfg` in it: it asks to initialize, is told the platform will not, and
/// draws the microphone as unavailable — which is the same path a desktop
/// with no input device takes.
#[derive(Default)]
pub struct AudioRecorder;

impl AudioRecorder {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// # Errors
    ///
    /// Always, on this platform. See the module documentation.
    pub fn init(&mut self) -> Result<(), RecorderError> {
        Err(unsupported())
    }

    /// # Errors
    ///
    /// Always, on this platform.
    pub fn start(&mut self) -> Result<(), RecorderError> {
        Err(unsupported())
    }

    #[must_use]
    pub fn level(&self) -> f32 {
        0.0
    }

    /// # Errors
    ///
    /// Always, on this platform.
    pub fn stop(&mut self) -> Result<RecordedAudio, RecorderError> {
        Err(RecorderError::NotRecording)
    }

    pub fn cancel(&mut self) {}
}

fn unsupported() -> RecorderError {
    RecorderError::DeviceError(
        "recording a voice note needs an Opus encoder, which this build does not have on the web"
            .to_string(),
    )
}

/// The mic half of a call.
///
/// # Errors
///
/// Always: calls are held by the process that owns the session, which is
/// never the page.
pub fn spawn_mic() -> anyhow::Result<async_channel::Receiver<Vec<i16>>> {
    anyhow::bail!("a page does not hold the session, so it does not open the microphone")
}

/// The speaker half of a call.
///
/// # Errors
///
/// Always, for the same reason as [`spawn_mic`].
pub fn spawn_speaker() -> anyhow::Result<async_channel::Sender<Vec<i16>>> {
    anyhow::bail!("a page does not hold the session, so it does not open the speaker")
}
