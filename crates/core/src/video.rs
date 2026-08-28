//! Call video: which directions are live, and the frames themselves.
//!
//! The library transports pre-encoded H.264 — one complete Annex-B access
//! unit per frame — so what crosses this boundary is a codec payload rather
//! than pixels. That is what makes a picture affordable between two
//! processes: a 720p frame is 3.5 MiB of RGBA and about 16 KiB encoded, and
//! the front end already owns an H.264 decoder for the video it plays in a
//! conversation.
//!
//! Both directions travel: the peer's because it is the call, and our own
//! because the camera is opened by the process that owns the session (the
//! same rule that puts the microphone there), so the window has no other way
//! to draw what it is sending. It is the *encoded* stream rather than a
//! second raw feed, which costs one more decode and no second encode, and has
//! the property that a self-view shows exactly what the peer is being sent.

use serde::{Deserialize, Serialize};

use super::call::CallId;

/// Which direction a frame belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoStream {
    /// Our camera, as the peer is being sent it.
    Local,
    /// The peer's camera.
    Remote,
}

/// Which of a call's two video directions are running.
///
/// Two independent flags rather than one "is a video call": WhatsApp lets
/// either side turn its camera on and off mid-call, and a call where only one
/// camera is on is the common case — someone answers a video call with video
/// off, or turns it off to save bandwidth. A single flag would draw the
/// remote pane for a peer sending nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallVideo {
    /// Our camera is open and its frames are going out.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub local: bool,
    /// The peer has video enabled and frames are expected from them.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub remote: bool,
    /// The peer has asked to turn this into a video call and is waiting on an
    /// answer.
    ///
    /// State rather than one window's memory of an event, because a window
    /// that attached after the request was made never saw it: it would draw
    /// an ordinary camera button while somebody waited for it, and learn
    /// nothing until the request timed out. The answer is a camera coming on,
    /// so it clears with `local` — but the *question* has to survive being
    /// asked before the asker was listening.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub requested: bool,
}

impl CallVideo {
    /// Whether anything at all is being drawn, which is what decides whether
    /// the card takes the video layout.
    ///
    /// A question is not a picture: a peer asking for video changes what the
    /// camera button says and not what the card is shaped like.
    #[must_use]
    pub fn any(self) -> bool {
        self.local || self.remote
    }

    /// The flag one direction owns, for a caller that has the direction in
    /// hand rather than a branch per field.
    #[must_use]
    pub fn is_on(self, stream: VideoStream) -> bool {
        match stream {
            VideoStream::Local => self.local,
            VideoStream::Remote => self.remote,
        }
    }

    /// Set one direction, returning whether it changed.
    pub fn set(&mut self, stream: VideoStream, on: bool) -> bool {
        let slot = match stream {
            VideoStream::Local => &mut self.local,
            VideoStream::Remote => &mut self.remote,
        };
        let changed = *slot != on;
        *slot = on;
        changed
    }
}

/// One encoded frame of a call's video, on its way to whoever draws it.
///
/// Deliberately not a `UiEvent`: an event is news that a client which missed
/// one has missed for good, and this is a stream where the *newest* frame is
/// the only one worth having. Everything on its path may drop it — a full
/// queue, a client that cannot keep up — and none of that is an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallVideoFrame {
    pub call_id: CallId,
    pub stream: VideoStream,
    /// One complete Annex-B access unit, start codes included.
    ///
    /// Base64 rather than a JSON array of numbers: the wire is
    /// newline-delimited JSON, and the array form is four bytes and a serde
    /// visitor call per byte. Base64 is a third the size of that and one pass
    /// over the buffer.
    #[serde(with = "crate::base64")]
    pub data: Vec<u8>,
    /// The unit carries an IDR — a decoder may (re)start here.
    pub keyframe: bool,
    /// Units were lost before this one.
    ///
    /// Carried *on the frame after the gap* rather than sent beside it,
    /// because a gap is a position in a stream and a message about one
    /// arrives somewhere else. A decoder told this stops and waits for a
    /// keyframe: what it holds no longer matches what the sender encoded
    /// against, and the units that follow reference what it never received.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub gap: bool,
    /// The sender's device rotation in quarter turns, clockwise.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub orientation: u8,
}

fn is_zero(value: &u8) -> bool {
    *value == 0
}

impl CallVideoFrame {
    #[must_use]
    pub fn new(
        call_id: CallId,
        stream: VideoStream,
        data: Vec<u8>,
        keyframe: bool,
        orientation: u8,
    ) -> Self {
        Self {
            call_id,
            stream,
            data,
            keyframe,
            orientation,
            gap: false,
        }
    }

    /// Say that units were lost before this one.
    #[must_use]
    pub fn after_a_gap(mut self, gap: bool) -> Self {
        self.gap = gap;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_direction_reads_back_what_was_set() {
        let mut video = CallVideo::default();
        assert!(!video.any());
        assert!(video.set(VideoStream::Remote, true));
        assert!(!video.set(VideoStream::Remote, true));
        assert!(video.any());
        assert!(!video.local);
        assert!(video.is_on(VideoStream::Remote));
    }

    #[test]
    fn a_frame_round_trips_its_payload() {
        let frame = CallVideoFrame::new(
            "call".to_string(),
            VideoStream::Local,
            vec![0, 0, 0, 1, 0x67, 0xff, 0xfe],
            true,
            3,
        );
        let json = serde_json::to_string(&frame).expect("serialize");
        assert_eq!(frame, serde_json::from_str(&json).expect("deserialize"));
    }

    /// The empty half of a frame is left out, and read back as what it was.
    #[test]
    fn an_omitted_orientation_comes_back_as_zero() {
        let frame = CallVideoFrame::new("c".into(), VideoStream::Remote, vec![1, 2], false, 0);
        let json = serde_json::to_string(&frame).expect("serialize");
        assert!(!json.contains("orientation"));
        assert_eq!(frame, serde_json::from_str(&json).expect("deserialize"));
    }
}
