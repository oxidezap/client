//! Camera capture and H.264 encoding for video calls. No UI, no IPC.
//!
//! The sibling of `oxidezap-audio`, and the same shape: a device opened on a
//! dedicated thread, feeding a channel the session hands straight to the
//! library. What crosses the boundary is *encoded* — the VoIP stack
//! transports pre-encoded H.264 access units and never touches a pixel — so
//! this crate owns the whole of the camera-to-bitstream path and nothing
//! above it needs to know what a webcam produces.
//!
//! It is deliberately not reusable as a video *player*: decoding belongs to
//! whoever draws, which on this side is the GPUI front end writing straight
//! into a `RenderImage`.

//! # Two backends
//!
//! nokhwa is three operating systems and OpenH264 is C, so neither reaches
//! `wasm32-unknown-unknown`. A browser has both anyway — `getUserMedia` is
//! the device and `VideoEncoder` is the codec — so the split is the same one
//! `oxidezap-audio` makes: one set of names, two implementations behind it,
//! and the same Annex-B access units out of either. See [`web`].

#[cfg(not(target_family = "wasm"))]
mod camera;
#[cfg(not(target_family = "wasm"))]
mod convert;
#[cfg(not(target_family = "wasm"))]
mod encoder;
#[cfg(target_family = "wasm")]
mod web;

// The same four names on both. `camera::open` used to be a fifth, exported
// only here: the blocking open the wrapper below wraps, with no web twin and
// no caller outside this file, so a crate that offers `open_camera` on both
// platforms also offered a second way in on one of them. It stays inside
// `camera`, where the wrapper reaches it.
#[cfg(not(target_family = "wasm"))]
pub use camera::{CameraControl, CameraStream, is_available};
#[cfg(target_family = "wasm")]
pub use web::{CameraControl, CameraStream, is_available, open_camera};

/// Open the camera, off the caller's thread.
///
/// The shape the browser backend has to have -- `getUserMedia` is a
/// permission prompt and answers no other way -- so the session's video plane
/// is written once. Here it is nokhwa's blocking open, moved somewhere
/// blocking is allowed.
///
/// # Errors
///
/// If no camera can be opened at `quality`.
#[cfg(not(target_family = "wasm"))]
pub async fn open_camera(quality: VideoQuality) -> anyhow::Result<CameraStream> {
    tokio::task::spawn_blocking(move || camera::open(quality))
        .await
        .map_err(|e| anyhow::anyhow!("camera task failed: {e}"))?
}

/// One encoded access unit.
///
/// Here rather than beside an encoder, because there are two encoders and one
/// of them is the browser's: a type that lived with OpenH264 could not be
/// named on the target that has no OpenH264, and the session names it on
/// both.
pub struct EncodedFrame {
    /// Annex-B, start codes included, exactly as the library's video source
    /// wants it.
    pub data: Vec<u8>,
    /// Carries an IDR: a decoder may start here.
    pub keyframe: bool,
}

#[cfg(not(target_family = "wasm"))]
use nokhwa::utils::FrameFormat;

/// What the camera is asked for and what the encoder aims at.
///
/// One struct because the four are not independent: the RTP timestamp stride
/// the library needs is `90000 / fps`, and a bitrate is only meaningful
/// against a resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoQuality {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

/// The 90 kHz clock every H.264 RTP profile counts in.
const VIDEO_CLOCK_RATE: u32 = 90_000;

/// What WhatsApp Web itself offers on a desktop: Constrained Baseline at
/// 720p20, a shade under 2 Mbps.
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
const DEFAULT_FPS: u32 = 20;
const DEFAULT_BITRATE_KBPS: u32 = 1980;

/// H.264 Level 3.1, which Constrained Baseline video calls are bounded by.
const LEVEL_31_MAX_FRAME_MBS: u32 = 3600;
const LEVEL_31_MAX_MBS_PER_SECOND: u32 = 108_000;
const MAX_BITRATE_KBPS: u32 = 14_000;

impl Default for VideoQuality {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            fps: DEFAULT_FPS,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
        }
    }
}

impl VideoQuality {
    /// The RTP clock increment between access units, which the library needs
    /// stated explicitly and which must match the pacing the camera is set
    /// to. Never zero: `fps` is bounded away from it by [`Self::checked`].
    #[must_use]
    pub fn timestamp_stride(self) -> u32 {
        VIDEO_CLOCK_RATE / self.fps.max(1)
    }

    /// The same numbers, refused if a peer could not decode them.
    ///
    /// Checked rather than clamped: these come from the environment, and a
    /// silently corrected setting is one whose author never learns it was
    /// wrong. The bounds are the level's, not ours.
    pub fn checked(self) -> anyhow::Result<Self> {
        use anyhow::bail;

        if self.width == 0
            || self.height == 0
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            bail!(
                "video size must be positive and even, got {}x{}",
                self.width,
                self.height
            );
        }
        if self.fps == 0 || self.fps > 60 || !VIDEO_CLOCK_RATE.is_multiple_of(self.fps) {
            bail!(
                "video frame rate must divide 90000 and be in 1..=60, got {}",
                self.fps
            );
        }
        if !(25..=MAX_BITRATE_KBPS).contains(&self.bitrate_kbps) {
            bail!(
                "video bitrate must be in 25..={MAX_BITRATE_KBPS} kbps, got {}",
                self.bitrate_kbps
            );
        }
        let frame_mbs = self
            .width
            .div_ceil(16)
            .saturating_mul(self.height.div_ceil(16));
        if frame_mbs > LEVEL_31_MAX_FRAME_MBS
            || frame_mbs.saturating_mul(self.fps) > LEVEL_31_MAX_MBS_PER_SECOND
        {
            bail!(
                "{}x{}@{} exceeds H.264 Level 3.1, which is what a video call may offer",
                self.width,
                self.height,
                self.fps
            );
        }
        Ok(self)
    }

    /// The default, with whatever the environment overrides.
    ///
    /// Four knobs and no settings pane: what a call should look like is not a
    /// decision a user has the information to make, and the one case that
    /// needs it — a machine too slow for 720p, a link too narrow for 2 Mbps —
    /// is one where a number in the environment is the right size of answer.
    #[must_use]
    pub fn from_environment() -> Self {
        let wanted = Self {
            width: env_pair("OXIDEZAP_VIDEO_SIZE").map_or(DEFAULT_WIDTH, |(w, _)| w),
            height: env_pair("OXIDEZAP_VIDEO_SIZE").map_or(DEFAULT_HEIGHT, |(_, h)| h),
            fps: env_u32("OXIDEZAP_VIDEO_FPS").unwrap_or(DEFAULT_FPS),
            bitrate_kbps: env_u32("OXIDEZAP_VIDEO_BITRATE_KBPS").unwrap_or(DEFAULT_BITRATE_KBPS),
        };
        match wanted.checked() {
            Ok(quality) => quality,
            Err(e) => {
                log::warn!("ignoring the video settings in the environment: {e}");
                Self::default()
            }
        }
    }
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn env_pair(name: &str) -> Option<(u32, u32)> {
    let value = std::env::var(name).ok()?;
    let (width, height) = value.trim().split_once(['x', 'X'])?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

/// A capture format's name, for a log line. The backend's own type has no
/// stable `Display` we want to depend on in messages.
#[cfg(not(target_family = "wasm"))]
fn format_name(format: FrameFormat) -> &'static str {
    match format {
        FrameFormat::MJPEG => "MJPEG",
        FrameFormat::YUYV => "YUYV",
        FrameFormat::NV12 => "NV12",
        FrameFormat::GRAY => "GRAY",
        FrameFormat::RAWRGB => "RGB",
        FrameFormat::RAWBGR => "BGR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_what_whatsapp_web_offers() {
        let quality = VideoQuality::default();
        assert!(quality.checked().is_ok());
        assert_eq!(quality.timestamp_stride(), 4_500);
    }

    #[test]
    fn a_size_a_peer_could_not_decode_is_refused() {
        let too_big = VideoQuality {
            width: 1920,
            height: 1080,
            ..VideoQuality::default()
        };
        assert!(too_big.checked().is_err());

        let odd = VideoQuality {
            width: 641,
            ..VideoQuality::default()
        };
        assert!(odd.checked().is_err());
    }

    /// The stride is what the library paces RTP by, and a rate that does not
    /// divide the clock would drift against it.
    #[test]
    fn a_frame_rate_that_does_not_divide_the_clock_is_refused() {
        for fps in [0, 7, 61] {
            let quality = VideoQuality {
                fps,
                ..VideoQuality::default()
            };
            assert!(quality.checked().is_err(), "{fps} fps should be refused");
        }
        for fps in [10, 15, 20, 30] {
            let quality = VideoQuality {
                fps,
                width: 640,
                height: 480,
                ..VideoQuality::default()
            };
            assert!(quality.checked().is_ok(), "{fps} fps should be allowed");
            assert_eq!(quality.timestamp_stride(), 90_000 / fps);
        }
    }
}
