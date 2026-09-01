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

/// How one call's audio endpoints ended, and on whose account: the engine
/// releasing a half, a microphone that went on this side, or a call that was
/// over before any engine received them. Portable: the rule is about channel
/// ends, so it is stated and tested off the browser.
pub mod call_ending;
mod encoder;
/// Opus packets, in the OGG stream WhatsApp expects. The container is not the
/// codec, and only the codec was ever the problem here.
mod ogg_opus;
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
/// owns the audio device -- which on the web is the page itself when it holds
/// no daemon, so `web::call_device` is the same bridge over WebAudio rather
/// than the refusal it used to be.
#[cfg(not(target_family = "wasm"))]
mod call_device;
#[cfg(target_family = "wasm")]
mod web;

/// What a call's audio is, in one place.
///
/// 16 kHz mono, 60 ms a frame, which is what the library's media plane takes
/// and hands back. No sound card runs at it -- cpal answers 44.1 or 48 and a
/// browser's `AudioContext` answers whatever the machine does -- so both
/// backends resample around these two numbers, and having them written twice
/// is how one backend ends up a frame out from the other.
pub(crate) const CALL_RATE: u32 = 16_000;
/// 60 ms at [`CALL_RATE`].
pub(crate) const CALL_FRAME_SAMPLES: usize = 960;

/// Whether a voice note can be recorded here.
///
/// Asked *before* the microphone is offered, not after it is opened: a
/// control that is drawn and then always fails is worse than one that is not
/// drawn.
///
/// A function rather than a constant, because on the web it is a question
/// about the browser rather than about the build — the encoder is
/// `AudioEncoder`, which an older one may not have — and the honest answer
/// there can only be given at runtime. On a desktop it is still settled when
/// the binary is.
#[must_use]
pub fn can_record() -> bool {
    #[cfg(not(target_family = "wasm"))]
    {
        true
    }
    #[cfg(target_family = "wasm")]
    {
        web::AudioRecorder::supported()
    }
}

pub use encoder::{EncoderError, encode_to_opus_ogg};
pub use player::PlayerError;
pub use recorder::{EncodedNote, RecordedAudio, RecorderError, Recording, TARGET_SAMPLE_RATE};
pub use waveform::{WAVEFORM_SAMPLES, generate_waveform};

// The same three names on both, which is the claim the header makes. Kept
// honest by being written as one list per platform rather than by trusting
// the sentence: `spawn_mic` and `spawn_speaker` used to be here too, with no
// caller outside `call_device` and no web twin, so the crate promised one API
// and exported two. They are the halves `open_call_audio` opens together --
// a call needs both or neither -- and they stay inside the backend that has
// them.
#[cfg(not(target_family = "wasm"))]
pub use call_device::open_call_audio;
#[cfg(not(target_family = "wasm"))]
pub use player::AudioPlayer;
#[cfg(not(target_family = "wasm"))]
pub use recorder::AudioRecorder;

#[cfg(target_family = "wasm")]
pub use web::{AudioPlayer, AudioRecorder, open_call_audio};
