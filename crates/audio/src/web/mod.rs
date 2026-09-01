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
//! - **Call devices**, which used to be the exception here and are not any
//!   more. The sentence was that the process owning the session owns the
//!   microphone and that process is the daemon — true of a page attached to
//!   an `oxidezapd`, and not of a page holding the session itself, which is
//!   the arrangement this build now has. See [`call_device`].
//!
//! What still does not survive is libopus, and nothing here needs it: a
//! call's codec is MLow, which is pure Rust in the library's own core.

mod call_device;
mod player;
mod recorder;

pub use call_device::open_call_audio;
pub use player::AudioPlayer;
pub use recorder::AudioRecorder;

/// The page's event loop, which is the only executor there is here.
///
/// Re-exported rather than written, so the two modules that spawn do not each
/// reach for `wasm_bindgen_futures` and drift on what they mean by it — and
/// so that this crate agrees with the rest of the workspace about what "hand
/// it to the loop" is. See `oxidezap_platform`.
pub(crate) use oxidezap_platform::spawn;
