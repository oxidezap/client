//! A call's video plane in a browser, which is no plane at all.
//!
//! The names [`super`] promises, so that nothing above learns which build it
//! is in — and [`open`] refusing, which is the one thing that actually
//! differs. Every caller of it already treats a camera that will not open as
//! an ordinary outcome: a video call whose device is busy is placed as a
//! voice call rather than not placed, and that is exactly the path a page
//! takes.
//!
//! There is no [`LocalVideo`] to construct, and nothing constructs one: the
//! registry that would hold cameras is the desktop registry, and a page's
//! never gets as far as a live call. The type exists because the session's
//! own code names it once, in a signature it shares with the desktop.

// Unused here, all of it, and that is what this module is: the names the
// desktop's call code needs so the session can be written once, on a target
// where nothing ever opens a camera to use them. The compiler is right and
// there is nothing to fix — the alternative is a `cfg` in the session's own
// logic, which is the thing this arrangement exists to avoid.
#![allow(dead_code)]

use std::sync::Arc;

use oxidezap_core::CallVideoFrame;
use portable_atomic::AtomicBool;

/// Where finished frames would go.
pub type VideoFrameSender = tokio::sync::mpsc::Sender<CallVideoFrame>;

/// The sender the daemon installs once and keeps.
pub type VideoSenderSlot = Arc<std::sync::Mutex<Option<VideoFrameSender>>>;

/// Where a finished frame goes, and the door in front of it.
///
/// Built and held exactly as on a desktop, and handed to an [`open`] that
/// always refuses — so nothing above has a second shape to write.
#[derive(Clone)]
pub struct VideoPublisher {
    pub(crate) sender: VideoSenderSlot,
    pub(crate) watched: Arc<AtomicBool>,
}

/// What a lost camera would be reported through.
pub(crate) type CameraLost = Arc<dyn Fn(String, CameraId) + Send + Sync>;

/// One opened camera, told apart from the next one on the same call.
pub(crate) type CameraId = u64;

/// How many frames may wait for the daemon.
///
/// The same number as the desktop's, because it sizes the daemon's own
/// subscription channel rather than anything a camera does.
pub(crate) const PUBLISH_DEPTH: usize = 4;

/// The call id a camera's frames are addressed to.
pub(crate) type CallIdSlot = Arc<std::sync::Mutex<String>>;

/// Nothing opens a camera here, but the id still has to be made the same way:
/// the caller builds one before it asks, and passes it in.
pub(crate) fn slot(call_id: &str) -> CallIdSlot {
    Arc::new(std::sync::Mutex::new(call_id.to_string()))
}

/// The camera, wired to a call — which in a browser is nothing at all.
///
/// Uninhabited in practice: [`open`] never returns one, so no caller ever
/// holds one to call these on.
pub(crate) struct LocalVideo {
    _never: std::convert::Infallible,
}

/// The two ends a call attaches to its media plane.
pub(crate) struct Endpoints {
    _never: std::convert::Infallible,
}

/// Refused, and said where it is asked rather than somewhere further in.
///
/// A camera needs `getUserMedia` and an encoder needs `VideoEncoder`; neither
/// is bound, so there is no picture a page could send. Every caller answers
/// this the way it answers a busy webcam — the call is placed or answered
/// without video — which is the behaviour a page should have anyway.
pub(crate) async fn open(
    _call_id: CallIdSlot,
    _publish: VideoPublisher,
    _lost: CameraLost,
) -> Result<(LocalVideo, Endpoints), String> {
    Err("a browser has no camera bound: video calls need the desktop app".to_string())
}
