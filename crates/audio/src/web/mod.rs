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
//! - **Recording**, through WebAudio for the capture and the browser's own
//!   Opus encoder for the codec. The container is written by
//!   [`crate::ogg_opus`], which is shared with the desktop, so the bytes a
//!   recipient gets come from one packager rather than two that would have to
//!   agree. See [`recorder`] for why `MediaRecorder` is the wrong route.
//!
//! - **Call devices.** The process that owns the WhatsApp session owns the
//!   microphone (see AGENTS.md), and on the web that process is the daemon.
//!   A page never opens either end of a call's audio, so these exist to keep
//!   one API across platforms and are never reached.

mod player;
mod recorder;

pub use player::AudioPlayer;
pub use recorder::AudioRecorder;

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
