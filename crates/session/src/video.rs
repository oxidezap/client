//! A call's video plane: the camera in, the peer's picture out.
//!
//! The session owns the camera for the same reason it owns the microphone —
//! it is the process holding the call — and the whole of what leaves this
//! module is *encoded*. That is what makes a picture affordable across the
//! daemon socket: 16 KiB of H.264 per frame rather than 3.5 MiB of pixels,
//! and the front end already carries a decoder for the video it plays in a
//! conversation.
//!
//! Both directions are published. The peer's because it is the call; our own
//! because nothing above this process has the camera, and re-encoding a
//! second preview stream would cost more than decoding the one already going
//! out. Sending exactly what the peer is sent also makes the self-view
//! honest: what is drawn is what they see, framing, freezes and all.
//!
//! Everything here is lossy by construction. A frame that cannot be delivered
//! *now* is worth nothing later, so every queue is short and every send is a
//! `try_send` — the one thing a drop must not do is leave the peer decoding
//! against a reference it never received, which is why the camera is asked
//! for a keyframe whenever one is lost.

use portable_atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use log::{debug, warn};
use oxidezap_core::{CallVideoFrame, VideoStream};
use oxidezap_video::{CameraControl, CameraStream, EncodedFrame, VideoQuality};
use whatsapp_rust::voip::{VideoFrame, VideoSource};

/// Where finished frames go on their way to whoever draws them.
///
/// Bounded and dropped from rather than blocked on: this is a stream, and the
/// only frame worth having is the newest one.
pub type VideoFrameSender = tokio::sync::mpsc::Sender<CallVideoFrame>;

/// The publisher as the pumps see it: a slot, read per frame.
///
/// Not a captured `VideoFrameSender`, because subscribing replaces it — and a
/// pump holding the old one would find its receiver closed and conclude that
/// nobody is watching, for the rest of the call, while a window sat in front
/// of it. Absent or closed means exactly "nobody is watching *now*", which is
/// a frame to drop and never a reason to stop pumping.
pub type VideoPublisher = Arc<std::sync::Mutex<Option<VideoFrameSender>>>;

/// What became of one frame handed to the publisher.
///
/// Three answers and not a `bool`, because only one of them is a *gap*:
/// nobody watching is the ordinary state of a daemon holding a call with its
/// window closed, and asking the encoder for a keyframe on every frame of it
/// would emit IDRs forever for no reader.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Sent,
    /// Nothing is drawing. Not a loss: there is nothing to recover.
    NoSubscriber,
    /// Somebody is drawing and could not keep up. The unit is gone, and what
    /// follows it references what they never got.
    Dropped,
}

/// Hand one frame to whoever is subscribed, if anyone is.
///
/// The frame is *built* by the caller's closure and only when there is
/// somewhere to send it: an access unit has to be copied out of the encoder's
/// buffer to travel, and nobody watching is the ordinary state of a daemon
/// holding a call with its window closed.
fn publish(publisher: &VideoPublisher, frame: impl FnOnce() -> CallVideoFrame) -> Delivery {
    let sender = publisher.lock().expect("video publisher poisoned").clone();
    let Some(sender) = sender else {
        return Delivery::NoSubscriber;
    };
    match sender.try_send(frame()) {
        Ok(()) => Delivery::Sent,
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Delivery::Dropped,
        // Between the clone and the send, the subscriber went away.
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Delivery::NoSubscriber,
    }
}

/// What is called when the device itself goes away.
///
/// A camera can be unplugged, or its backend can fail for good, and the
/// capture thread then ends on its own. Nothing else notices: the call runs
/// on, the registry still holds a camera, and every window goes on drawing a
/// direction that will never produce another frame. A deliberate stop does
/// not come through here — that path aborts the pump before the channel
/// closes — so this means exactly "the device is gone".
///
/// Carries which camera died, not only which call it was on. The cleanup is
/// spawned, so a user who turns video off and on again in that window would
/// otherwise have the *replacement* torn down by the failure of the one
/// before it.
pub(crate) type CameraLost = Arc<dyn Fn(String, CameraId) + Send + Sync>;

/// One opened camera, told apart from the next one on the same call.
///
/// A counter rather than the device's own name: two opens of one webcam are
/// two cameras as far as a call is concerned, and what has to be answerable
/// is "is the thing in the registry still the thing that failed".
pub(crate) type CameraId = u64;

fn next_camera_id() -> CameraId {
    static NEXT: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, portable_atomic::Ordering::Relaxed)
}

/// How many frames may wait for the daemon. Small: a backlog here is latency
/// the person on screen can see.
pub(crate) const PUBLISH_DEPTH: usize = 4;

/// How many encoded units may wait for the media plane, and how many decoded
/// ones for the front end.
const PLANE_DEPTH: usize = 2;

/// The camera, wired to a call.
///
/// Held for as long as the local direction is on: dropping it stops the
/// device.
pub(crate) struct LocalVideo {
    /// Owned outright, not shared: closing the device is a matter of waiting
    /// for its thread, and a second owner would leave nothing able to wait.
    /// What the pump needs is the *control*, which is shareable.
    camera: CameraStream,
    /// Whether the self-view is worth sending yet.
    ///
    /// The camera opens before the offer goes out — it has to, or the offer
    /// is not a video offer — and a call can then ring for half a minute. A
    /// window has no live call to draw those frames into, so publishing them
    /// would base64 a 720p stream across the socket, spin up a decoder and
    /// convert every frame to pixels, all of it to be thrown away on arrival.
    drawable: Arc<AtomicBool>,
    /// The fan-out task, stopped by dropping the camera's channel.
    pump: tokio::task::JoinHandle<()>,
    /// Cleared by the pump *before* it reports the loss, so a caller still
    /// wiring this camera up can ask whether the device is still there.
    ///
    /// The report is what a registered camera is torn down by, and it finds
    /// nothing to tear down while the camera is still on its way into the
    /// registry — seconds, on a path that waits for signaling. Whoever is
    /// holding it asks here instead.
    alive: Arc<AtomicBool>,
    id: CallIdSlot,
    camera_id: CameraId,
}

/// Which call the frames belong to, as a slot rather than a value.
///
/// An outgoing call is named twice — the window's placeholder, then the id
/// the server answers with — and the camera opens before the first frame of
/// that exchange, because the offer has to *be* a video offer. So the pumps
/// read the id per frame instead of capturing it, and the rename lands
/// without restarting the device.
pub(crate) type CallIdSlot = Arc<std::sync::Mutex<String>>;

pub(crate) fn slot(call_id: &str) -> CallIdSlot {
    Arc::new(std::sync::Mutex::new(call_id.to_string()))
}

fn read(id: &CallIdSlot) -> String {
    id.lock().expect("call id slot poisoned").clone()
}

impl LocalVideo {
    /// There is a live call now, so the self-view has somewhere to land.
    pub(crate) fn drawable(&self) {
        self.drawable.store(true, Ordering::Relaxed);
    }

    /// Which opened camera this is, so a teardown scheduled for an earlier
    /// one does not take it down.
    pub(crate) fn camera_id(&self) -> CameraId {
        self.camera_id
    }

    /// Whether the device is still producing. False means its loss has
    /// already been reported — to a registry this camera was not in yet.
    pub(crate) fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Address this call's frames by the name the server gave it.
    pub(crate) fn rename(&self, call_id: &str) {
        *self.id.lock().expect("call id slot poisoned") = call_id.to_string();
    }

    /// Tell the encoder the peer has lost the stream.
    pub(crate) fn request_keyframe(&self) {
        self.camera.control().request_keyframe();
    }

    /// Close the device and wait for the thread to let go of it.
    ///
    /// Waited for because the next call opens the same camera, and a backend
    /// that still holds it fails that open rather than queueing behind it.
    pub(crate) async fn stop(self) {
        // Aborting the pump is a request, not a fact — the task may not have
        // been polled yet — so the device is closed by the owner rather than
        // by whoever happens to drop the last reference.
        self.pump.abort();
        let camera = self.camera;
        // On a blocking thread: closing waits for the frame the capture
        // thread is asleep in.
        let _ = tokio::task::spawn_blocking(move || camera.stop()).await;
    }
}

/// What the library is handed for one call's video.
pub(crate) struct Endpoints {
    pub(crate) source: CameraSource,
    pub(crate) sink: async_channel::Sender<VideoFrame>,
}

/// The camera as a [`VideoSource`].
///
/// A bare channel would already satisfy the trait, and would also claim the
/// default 15 fps stride. The stride is what paces RTP, so a camera opened at
/// 20 fps under a 15 fps stride drifts against its own timestamps — hence a
/// named type whose whole purpose is to state the one the device is actually
/// running at.
pub(crate) struct CameraSource {
    frames: async_channel::Receiver<Vec<u8>>,
    stride: u32,
}

impl VideoSource for CameraSource {
    fn frames(&self) -> async_channel::Receiver<Vec<u8>> {
        self.frames.clone()
    }

    fn rtp_timestamp_stride(&self) -> u32 {
        self.stride
    }
}

/// Open the camera and wire both directions up for `call_id`.
///
/// The open is blocking (every capture backend is) and is done here rather
/// than by the caller so that a machine with no camera fails *before* an
/// offer or an accept goes out claiming video.
pub(crate) async fn open(
    call_id: CallIdSlot,
    publisher: VideoPublisher,
    lost: CameraLost,
) -> Result<(LocalVideo, Endpoints), String> {
    let quality = VideoQuality::from_environment();
    let camera = tokio::task::spawn_blocking(move || oxidezap_video::open(quality))
        .await
        .map_err(|e| format!("camera task failed: {e}"))?
        .map_err(|e| format!("{e:#}"))?;
    let stride = camera.quality().timestamp_stride();
    let camera_id = next_camera_id();
    let drawable = Arc::new(AtomicBool::new(false));
    let alive = Arc::new(AtomicBool::new(true));

    // The encoder's own queue is upstream of this one; this pair is what the
    // media plane and the front end read.
    let (source_tx, source_rx) = async_channel::bounded(PLANE_DEPTH);
    let (sink_tx, sink_rx) = async_channel::bounded(PLANE_DEPTH);

    let pump = tokio::spawn(pump_local(LocalPump {
        call_id: Arc::clone(&call_id),
        frames: camera.frames(),
        camera: camera.control(),
        plane: source_tx,
        publisher: Arc::clone(&publisher),
        lost,
        camera_id,
        drawable: Arc::clone(&drawable),
        alive: Arc::clone(&alive),
    }));
    tokio::spawn(pump_remote(Arc::clone(&call_id), sink_rx, publisher));

    Ok((
        LocalVideo {
            camera,
            pump,
            id: call_id,
            camera_id,
            drawable,
            alive,
        },
        Endpoints {
            source: CameraSource {
                frames: source_rx,
                stride,
            },
            sink: sink_tx,
        },
    ))
}

/// Camera to the media plane, and to the self-view.
///
/// One reader, two destinations: the plane must not be starved by a front end
/// that is not reading, and a front end must not hold the plane up. Both are
/// `try_send`, and only the plane's drop is worth a keyframe — a self-view
/// that misses a frame recovers on the next one it does get.
/// The ends the local pump is tied to, named rather than listed: eight
/// positional arguments is a call nobody can read and one nobody can get
/// wrong twice.
pub(crate) struct LocalPump {
    call_id: CallIdSlot,
    frames: async_channel::Receiver<EncodedFrame>,
    camera: CameraControl,
    plane: async_channel::Sender<Vec<u8>>,
    publisher: VideoPublisher,
    lost: CameraLost,
    camera_id: CameraId,
    drawable: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
}

async fn pump_local(pump: LocalPump) {
    let LocalPump {
        call_id,
        frames,
        camera,
        plane,
        publisher,
        lost,
        camera_id,
        drawable,
        alive,
    } = pump;
    // Set by a drop, spent on the next frame that gets through — the one
    // whose references are the ones missing. See `CallVideoFrame::gap`.
    let mut gap = false;
    while let Ok(EncodedFrame { data, keyframe }) = frames.recv().await {
        // Nothing is drawing a call that is still ringing. The camera runs
        // regardless — the offer said it would — but the picture goes
        // nowhere until there is somewhere to put it.
        if drawable.load(Ordering::Relaxed) {
            let drawn = publish(&publisher, || {
                CallVideoFrame::new(
                    read(&call_id),
                    VideoStream::Local,
                    data.clone(),
                    keyframe,
                    0,
                )
                .after_a_gap(gap)
            });
            // The self-view lost a unit, and cannot say so itself: the window
            // never sees what did not arrive. One extra IDR — on a stream
            // that emits one every few seconds anyway — against a self-view
            // frozen until the next, and the mark travels with the frame that
            // does arrive.
            gap = drawn == Delivery::Dropped;
            if gap {
                camera.request_keyframe();
            }
        }
        if plane.try_send(data).is_err() {
            if plane.is_closed() {
                break;
            }
            // The plane is behind. Whatever it sends next has to be
            // decodable on its own, since everything after a gap references
            // a unit the peer never received.
            camera.request_keyframe();
        }
    }
    // Reached only by the camera's own channel closing, which is the device
    // ending the stream rather than anyone asking it to: a deliberate stop
    // aborts this task first. The plane going away is the other way out, and
    // that one is the call ending, which has its own teardown.
    let call_id = read(&call_id);
    debug!("local video for {call_id} ended");
    if !plane.is_closed() {
        warn!("the camera on call {call_id} stopped producing frames");
        // Before the report, not after: the report tears down what the
        // registry holds, and a camera still being wired into it is not
        // there to be found. The flag is what that caller reads.
        alive.store(false, Ordering::Relaxed);
        lost(call_id, camera_id);
    }
}

/// The peer's picture, on its way to whoever draws it.
async fn pump_remote(
    call_id: CallIdSlot,
    frames: async_channel::Receiver<VideoFrame>,
    publisher: VideoPublisher,
) {
    // Runs for as long as the call does, whoever is or is not watching. A
    // pump that stopped at the first frame nobody took would leave the peer's
    // picture gone for good the moment a window closed and reopened.
    //
    // A dropped unit here is one the far side has to recover from, and there
    // is nothing on this side that can ask it to: the library parses the
    // peer's PLI and FIR but exposes no way to *send* one, so the peer's own
    // periodic keyframe is what ends the gap.
    let mut gap = false;
    while let Ok(frame) = frames.recv().await {
        let drawn = publish(&publisher, || {
            CallVideoFrame::new(
                read(&call_id),
                VideoStream::Remote,
                frame.data,
                frame.keyframe,
                frame.orientation,
            )
            .after_a_gap(gap)
        });
        // Nothing here can ask the peer for a keyframe, so the most this can
        // do is tell the decoder not to draw on what it no longer has.
        gap = drawn == Delivery::Dropped;
    }
    debug!("remote video for {} ended", read(&call_id));
}
