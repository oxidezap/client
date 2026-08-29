//! Audio for voice messages and calls.
//!
//! - Capture, Opus encoding into an OGG container, and waveform generation
//!   for WhatsApp PTT messages
//! - Playback of received voice notes and of a video's sound
//! - The mic/speaker bridge for VoIP calls (the engine lives in the library)
//!
//! # Two backends
//!
//! Everything above was cpal and libopus, and a page has neither: cpal's own
//! WebAudio backend does compile, but libopus is C and `wasm32-unknown-unknown`
//! has no C toolchain behind it. Rather than lose the crate on that platform,
//! the parts that need an operating system are gathered behind `cfg` and a
//! [`web`] backend answers in the same vocabulary — with playback genuinely
//! implemented, because the browser decodes Opus itself, and recording
//! refused up front, because nothing here could encode what it captured.
//!
//! A caller sees one API either way. `oxidezap-gui` has no `cfg` in it about
//! sound.

mod encoder;
mod player;
mod recorder;
mod resample;
mod waveform;

/// Pitch-preserving re-timing, for the player that does its own mixing. A
/// browser re-times with `playbackRate`, which resamples instead — see
/// `web::AudioPlayer::set_speed`.
#[cfg(not(target_family = "wasm"))]
mod timescale;

/// The cpal mic/speaker bridge for calls. The process that owns the session
/// owns the audio device, and that is never the page.
#[cfg(not(target_family = "wasm"))]
mod call_device;
#[cfg(target_family = "wasm")]
mod web;

/// Whether this build can record a voice note.
///
/// Asked *before* the microphone is offered, not after it is opened: a
/// control that is drawn and then always fails is worse than one that is not
/// drawn. False on the web, where there is no Opus encoder — see [`web`].
pub const CAN_RECORD: bool = cfg!(not(target_family = "wasm"));

pub use encoder::{EncoderError, encode_to_opus_ogg};
pub use player::PlayerError;
pub use recorder::{RecordedAudio, RecorderError, TARGET_SAMPLE_RATE};
pub use waveform::{WAVEFORM_SAMPLES, generate_waveform};

#[cfg(not(target_family = "wasm"))]
pub use call_device::{spawn_mic, spawn_speaker};
#[cfg(not(target_family = "wasm"))]
pub use player::AudioPlayer;
#[cfg(not(target_family = "wasm"))]
pub use recorder::AudioRecorder;

#[cfg(target_family = "wasm")]
pub use web::{AudioPlayer, AudioRecorder, spawn_mic, spawn_speaker};
