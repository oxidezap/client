//! A call's video plane: the camera in, the peer's picture out.
//!
//! Written once for both platforms. It used to be a split -- a real plane on
//! a desktop and, on the web, the names it promises with an `open` that
//! always refused -- and the reason was never this file: it was that nokhwa
//! is three operating systems and OpenH264 is C. Neither is true of a
//! browser's own camera and encoder, so `oxidezap-video` grew the second
//! backend and this became one implementation again.
//!
//! What is left of the platform here is where its work runs
//! ([`crate::exec`]) and how a pump is stopped. A page's spawned task cannot
//! be aborted, so nothing here aborts one: teardown closes the channels the
//! pumps read, which ends them at their next `recv` and is what the abort was
//! approximating anyway.
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
use oxidezap_video::{CameraStream, EncodedFrame, VideoQuality};

use crate::exec::Task;
use whatsapp_rust::voip::{VideoFrame, VideoSource};

/// Where finished frames go on their way to whoever draws them.
///
/// Bounded and dropped from rather than blocked on: this is a stream, and the
/// only frame worth having is the newest one.
pub type VideoFrameSender = tokio::sync::mpsc::Sender<CallVideoFrame>;

/// Where a finished frame goes, and the door in front of it.
///
/// The sender is a slot read per frame rather than a captured
/// `VideoFrameSender`, because subscribing replaces it — and a pump holding
/// the old one would find its receiver closed and conclude that nobody is
/// watching, for the rest of the call, while a window sat in front of it.
/// Absent or closed means exactly "nobody is watching *now*", which is a
/// frame to drop and never a reason to stop pumping.
#[derive(Clone)]
pub struct VideoPublisher {
    pub(crate) sender: VideoSenderSlot,
    /// Whether anybody is drawing at all, which the daemon owns: the sender
    /// belongs to the process and outlives every window, so the slot alone
    /// cannot say. Read before the frame is built, because building one
    /// copies an access unit out of the encoder's buffer.
    pub(crate) watched: Arc<AtomicBool>,
}

/// The sender the daemon installs once and keeps.
pub type VideoSenderSlot = Arc<std::sync::Mutex<Option<VideoFrameSender>>>;

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

impl Delivery {
    /// Whether a gap is still owed to whoever draws next.
    ///
    /// Only a frame that arrived spends the mark. A slot nobody was
    /// listening to carried nothing, so clearing the mark there hands the
    /// next frame to a decoder still missing the units before it, with
    /// nothing on it to say so.
    fn still_owes_a_gap(self, pending: bool) -> bool {
        match self {
            Delivery::Sent => false,
            Delivery::Dropped => true,
            Delivery::NoSubscriber => pending,
        }
    }
}

/// Hand one frame to whoever is subscribed, if anyone is.
///
/// The frame is *built* by the caller's closure and only when there is
/// somewhere to send it: an access unit has to be copied out of the encoder's
/// buffer to travel, and nobody watching is the ordinary state of a daemon
/// holding a call with its window closed.
fn publish(publisher: &VideoPublisher, frame: impl FnOnce() -> CallVideoFrame) -> Delivery {
    if !publisher.watched.load(Ordering::Relaxed) {
        return Delivery::NoSubscriber;
    }
    let sender = publisher
        .sender
        .lock()
        .expect("video publisher poisoned")
        .clone();
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

/// Requests cleanup when capture or its outbound endpoint ends unexpectedly.
///
/// Endpoint loss does not imply device failure. An upgrade timeout can release
/// the endpoint while the registry still owns a working camera. Explicit stop
/// and owner drop suppress this callback. The callback must schedule cleanup,
/// not wait for it, since cleanup joins the pump calling it.
///
/// Carries which camera died, not only which call it was on. The cleanup is
/// spawned, so a user who turns video off and on again in that window would
/// otherwise have the *replacement* torn down by the failure of the one
/// before it.
///
/// The bound is the platform's, and it is the same difference [`crate::exec`]
/// names once: what this closure captures is the call registry, and on a page
/// that holds the library's own `CallHandle`, which is not `Send` there and
/// does not need to be. A cfg on the alias rather than a `MaybeSendSync`
/// bound, because only auto traits may be added to a trait object.
#[cfg(not(target_family = "wasm"))]
pub(crate) type CameraLost = Arc<dyn Fn(String, CameraId) + Send + Sync>;
/// See the desktop half: on a page the bound is empty.
#[cfg(target_family = "wasm")]
pub(crate) type CameraLost = Arc<dyn Fn(String, CameraId)>;

/// Called when the peer's picture is dropped on this side, so the peer can be
/// asked for the keyframe that ends the gap.
///
/// A callback rather than a handle, for the same reason [`CameraLost`] is one:
/// a camera is opened *before* the call handle exists on the accept path, so
/// there is nothing to hold yet. Resolving the call by id when the drop happens
/// finds whatever is live by then, which on that path is the handle this open
/// was for.
#[cfg(not(target_family = "wasm"))]
pub(crate) type PictureLost = Arc<dyn Fn(&str) + Send + Sync>;
/// See the desktop half: on a page the bound is empty.
#[cfg(target_family = "wasm")]
pub(crate) type PictureLost = Arc<dyn Fn(&str)>;

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

/// The NAL types inside one Annex-B access unit, in order: 7 is an SPS, 8
/// a PPS, 5 a slice. Read for the log, so an IDR going out without its
/// parameter sets is one line rather than a silent non-starter.
fn idr_nal_types(data: &[u8]) -> Vec<u8> {
    use whatsapp_rust::wacore::voip::h264::{nal_unit_type, split_annexb};

    split_annexb(data).map(nal_unit_type).collect()
}

/// The first SPS and PPS NALs of an access unit, hex, without start codes.
/// A decoder configures off exactly these bytes, so two calls whose IDRs
/// carry different sets are different streams even when the NAL type lists
/// match — and the log can tell them apart.
fn idr_parameter_sets(data: &[u8]) -> (String, String) {
    use whatsapp_rust::wacore::voip::h264::{nal_unit_type, split_annexb};

    let mut sps = String::from("none");
    let mut pps = String::from("none");
    for nal in split_annexb(data) {
        let entry = match nal_unit_type(nal) {
            7 if sps == "none" => &mut sps,
            8 if pps == "none" => &mut pps,
            _ => continue,
        };
        *entry = nal.iter().map(|byte| format!("{byte:02x}")).collect();
    }
    (sps, pps)
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
    /// Taken during explicit stop so owner drop can still retire the channels.
    camera: Option<CameraStream>,
    /// Whether this side's picture has anywhere to go yet.
    ///
    /// The camera opens before the offer goes out — it has to, or the offer
    /// is not a video offer — and a call can then ring for half a minute.
    /// Neither destination wants those frames. A window has no live call to
    /// draw them into, so publishing them would base64 a 720p stream across
    /// the socket, spin up a decoder and convert every frame to pixels, all
    /// of it to be thrown away on arrival. And the peer discards them too:
    /// it opens its pane off the offer and the media, not off a standalone
    /// announcement — captured video-from-start calls carry none — so a unit
    /// that arrives before the peer's pane exists is decoded by nobody,
    /// after paying for its place in a relay channel whose congestion
    /// window has only just opened.
    live: Arc<AtomicBool>,
    /// The fan-out task, stopped by closing the camera's channel.
    ///
    /// Taken when joined: the same owner is stopped in two stages
    /// (`stop_local`, then `stop`), and a task joined twice panics.
    pump: Option<Task<()>>,
    /// The camera's own frame channel, so [`Self::stop`] can end the pump on
    /// a page, where a spawned task cannot be aborted.
    frames: async_channel::Receiver<EncodedFrame>,
    /// Told to the pump before its channel closes; see [`LocalPump::stopping`].
    stopping: Arc<AtomicBool>,
    /// The peer's half of the same `Endpoints` pair, held for the same
    /// reason: nothing else here can end it, and one that outlived the pair
    /// publishes into whatever call the id slot names next.
    remote_pump: Option<Task<()>>,
    /// The peer half's channel, closed for the same reason as `frames`.
    sink: async_channel::Receiver<VideoFrame>,
    /// Whether this camera is still producing, cleared by the pump on every
    /// way out of it.
    ///
    /// A caller still wiring the camera up asks here, because the loss report
    /// tears down what the *registry* holds and finds nothing while the
    /// camera is on its way into it — seconds, on a path that waits for
    /// signaling. So the flag is about the pump and not about the report:
    /// the plane closing is an ending too, and leaving the flag
    /// set for it hands a caller a camera it will draw as live and never
    /// receive another frame from.
    alive: Arc<AtomicBool>,
    id: CallIdSlot,
    camera_id: CameraId,
    /// Retained so pump exit alone cannot close the source channel. Closure
    /// here means the library released or closed its receiver.
    plane: async_channel::Sender<Vec<u8>>,
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
    pub(crate) fn live(&self) {
        self.live.store(true, Ordering::Relaxed);
    }

    /// Which opened camera this is, so a teardown scheduled for an earlier
    /// one does not take it down.
    pub(crate) fn camera_id(&self) -> CameraId {
        self.camera_id
    }

    /// Whether the device is still producing. False means this camera has
    /// stopped, whether or not its loss was worth reporting.
    pub(crate) fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed) && !self.frames.is_closed() && !self.endpoint_closed()
    }

    /// Name an ending asked-for before carrying it out, so a pump that
    /// observes the teardown first reads it the way [`Self::stop`] would
    /// have told it: withdrawing an upgrade stops the peer stanza before
    /// closing the device, and the endpoint release in between must not
    /// report the camera that release was for as lost.
    pub(crate) fn mark_stopping(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    pub(crate) fn endpoint_closed(&self) -> bool {
        self.plane.is_closed()
    }

    /// Address this call's frames by the name the server gave it.
    pub(crate) fn rename(&self, call_id: &str) {
        *self.id.lock().expect("call id slot poisoned") = call_id.to_string();
    }

    /// Tell the encoder the peer has lost the stream.
    pub(crate) fn request_keyframe(&self) {
        if let Some(camera) = &self.camera {
            camera.control().request_keyframe();
        }
    }

    /// Retire only this side's capture and its local pump, keeping the
    /// peer's pump and sink attached.
    ///
    /// The library now gates outbound off while inbound keeps decoding when
    /// our camera stops, so ending the shared remote half here would do what
    /// the network no longer does: freeze the picture we are still being
    /// sent. Full call teardown still goes through [`Self::stop`], which
    /// retires both halves together.
    pub(crate) async fn stop_local(mut self) -> LocalVideo {
        self.stopping.store(true, Ordering::Relaxed);
        self.frames.close();
        if let Some(camera) = self.camera.take() {
            camera.stop().await;
        }
        if let Some(pump) = self.pump.take() {
            let _ = pump.await;
        }
        self
    }

    /// Close the device and wait for the thread to let go of it.
    ///
    /// Waited for because the next call opens the same camera, and a backend
    /// that still holds it fails that open rather than queueing behind it.
    pub(crate) async fn stop(mut self) {
        // Said before the channel closes, and that order is the whole point:
        // the pump reads this on its way out to tell a device that died from
        // one that was asked to stop, and the two reach it as the same closed
        // channel.
        self.stopping.store(true, Ordering::Relaxed);
        // Closed rather than aborted. An abort is a request the task may
        // never be polled to hear, and on a page there is no abort at all --
        // a `spawn_local` task is cancelled by nothing. Closing the channel a
        // pump is parked in ends it at the `recv` it is already sitting in,
        // on both platforms and without a second mechanism.
        self.frames.close();
        // The peer's pump too, and for the reason the local one is ended: it
        // is the other half of one `Endpoints` pair, and the only thing that
        // would end it otherwise is the library dropping the sink. One that
        // outlived its pair would go on publishing `VideoStream::Remote`
        // under whatever call id the slot holds next, interleaved with the
        // new call's own pump.
        self.sink.close();
        // Waited for, because the next call opens the same device and a
        // backend that still holds it fails that open rather than queueing
        // behind it. Where that wait is a blocking one, the camera itself is
        // what moves it off a runtime thread.
        if let Some(camera) = self.camera.take() {
            camera.stop().await;
        }
        // And the pumps, so a camera reported as stopped is one that has
        // stopped: a pump still draining its queue can publish a frame after
        // the call that owned it is gone. Taken, because `stop_local` may
        // already have joined the local half.
        if let Some(pump) = self.pump.take() {
            let _ = pump.await;
        }
        if let Some(remote_pump) = self.remote_pump.take() {
            let _ = remote_pump.await;
        }
    }
}

impl Drop for LocalVideo {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        self.alive.store(false, Ordering::Relaxed);
        self.frames.close();
        self.sink.close();
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
    picture_lost: PictureLost,
) -> Result<(LocalVideo, Endpoints), String> {
    let quality = VideoQuality::from_environment();
    let camera = oxidezap_video::open_camera(quality)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let stride = camera.quality().timestamp_stride();
    let camera_id = next_camera_id();
    let live = Arc::new(AtomicBool::new(false));
    let alive = Arc::new(AtomicBool::new(true));

    // The encoder's own queue is upstream of this one; this pair is what the
    // media plane and the front end read.
    let (source_tx, source_rx) = async_channel::bounded(PLANE_DEPTH);
    let (sink_tx, sink_rx) = async_channel::bounded(PLANE_DEPTH);

    let frames = camera.frames();
    let stopping = Arc::new(AtomicBool::new(false));
    let control = camera.control();
    let pump = crate::exec::spawn(pump_local(LocalPump {
        call_id: Arc::clone(&call_id),
        frames: frames.clone(),
        request_keyframe: move || control.request_keyframe(),
        plane: source_tx.clone(),
        publisher: publisher.clone(),
        lost,
        camera_id,
        live: Arc::clone(&live),
        alive: Arc::clone(&alive),
        stopping: Arc::clone(&stopping),
    }));
    let remote_pump = crate::exec::spawn(pump_remote(
        Arc::clone(&call_id),
        sink_rx.clone(),
        publisher,
        picture_lost,
        Arc::clone(&stopping),
    ));

    Ok((
        LocalVideo {
            camera: Some(camera),
            pump: Some(pump),
            frames,
            remote_pump: Some(remote_pump),
            sink: sink_rx,
            id: call_id,
            camera_id,
            live,
            alive,
            stopping,
            plane: source_tx,
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

#[cfg(any(test, all(feature = "test-support", not(target_family = "wasm"))))]
/// Real pumps without a capture device. The sender can simulate capture loss,
/// and dropping the endpoints simulates library teardown.
pub(crate) fn camera_fixture(
    call_id: &str,
    lost: CameraLost,
) -> (LocalVideo, Endpoints, async_channel::Sender<EncodedFrame>) {
    let (capture, frames) = async_channel::bounded(PLANE_DEPTH);
    let (plane, source) = async_channel::bounded(PLANE_DEPTH);
    let (sink_tx, sink) = async_channel::bounded(PLANE_DEPTH);
    let id = slot(call_id);
    let camera_id = next_camera_id();
    let alive = Arc::new(AtomicBool::new(true));
    let live = Arc::new(AtomicBool::new(false));
    let stopping = Arc::new(AtomicBool::new(false));
    let publisher = VideoPublisher {
        sender: Arc::new(std::sync::Mutex::new(None)),
        watched: Arc::new(AtomicBool::new(false)),
    };
    let pump = crate::exec::spawn(pump_local(LocalPump {
        call_id: id.clone(),
        frames: frames.clone(),
        request_keyframe: || {},
        plane: plane.clone(),
        publisher: publisher.clone(),
        lost,
        camera_id,
        live: live.clone(),
        alive: alive.clone(),
        stopping: stopping.clone(),
    }));
    let remote_pump = crate::exec::spawn(pump_remote(
        id.clone(),
        sink.clone(),
        publisher,
        Arc::new(|_| {}),
        stopping.clone(),
    ));
    (
        LocalVideo {
            camera: None,
            live,
            pump: Some(pump),
            frames,
            stopping,
            remote_pump: Some(remote_pump),
            sink,
            alive,
            id,
            camera_id,
            plane,
        },
        Endpoints {
            source: CameraSource {
                frames: source,
                stride: 6000,
            },
            sink: sink_tx,
        },
        capture,
    )
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
struct LocalPump<F> {
    call_id: CallIdSlot,
    frames: async_channel::Receiver<EncodedFrame>,
    request_keyframe: F,
    plane: async_channel::Sender<Vec<u8>>,
    publisher: VideoPublisher,
    lost: CameraLost,
    camera_id: CameraId,
    live: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    /// Set by [`LocalVideo::stop`] before it closes the camera's channel.
    ///
    /// Capture failure and explicit stop both close `frames`. Only an ending
    /// that was not requested should schedule cleanup.
    stopping: Arc<AtomicBool>,
}

async fn pump_local(pump: LocalPump<impl Fn()>) {
    let LocalPump {
        call_id,
        frames,
        request_keyframe,
        plane,
        publisher,
        lost,
        camera_id,
        live,
        alive,
        stopping,
    } = pump;
    // Set by a drop, spent on the next frame that gets through — the one
    // whose references are the ones missing. See `CallVideoFrame::gap`.
    let mut gap = false;
    // Firsts and totals, for the reason the relay reports them: the two hops
    // below fail by doing nothing, and a self-view nobody is drawing, a plane
    // that refuses every unit, and an encoder that never produced one are the
    // same silence in a log and three unrelated faults. See `Capture` in the
    // video crate, which covers the hops upstream of this one.
    let mut taken = 0u64;
    let mut to_the_plane = 0u64;
    let mut refused_by_the_plane = 0u64;
    let mut drawn = 0u64;
    let mut dropped_by_the_window = 0u64;
    let mut unwatched = false;
    loop {
        let frame = tokio::select! {
            biased;
            _ = plane.closed() => break,
            frame = frames.recv() => frame,
        };
        let Ok(EncodedFrame { data, keyframe }) = frame else {
            break;
        };
        if stopping.load(Ordering::Relaxed) {
            break;
        }
        taken += 1;
        if taken == 1 {
            debug!("the first encoded frame of local video reached the session");
        }
        // One of the pump's three endings, and it has to be noticed whether
        // or not this frame was going anywhere.
        if plane.is_closed() {
            break;
        }
        // Nothing draws a call that is still ringing, and nothing decodes one
        // either. The camera runs regardless — the offer said it would — but
        // both destinations throw the picture away until `live`, so until
        // then the pump's whole job is to keep the channel drained. See the
        // field's own comment for what each end does with an early frame.
        if !live.load(Ordering::Relaxed) {
            continue;
        }
        // What the peer's decoder has to work with, on every keyframe: an
        // IDR without its parameter sets starts nothing, and without this
        // line the log cannot tell one from a complete unit — or two
        // complete units with different sets from each other. Read now,
        // said only once the plane takes the unit: a refused IDR is not
        // available to the peer however complete it looks.
        let idr_audit = keyframe.then(|| {
            let (sps, pps) = idr_parameter_sets(&data);
            format!("{:?} sps={sps} pps={pps}", idr_nal_types(&data))
        });
        {
            let delivery = publish(&publisher, || {
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
            if delivery == Delivery::Dropped {
                request_keyframe();
            }
            match delivery {
                Delivery::Sent => {
                    drawn += 1;
                    if drawn == 1 {
                        debug!("the first frame of local video was handed to the window");
                    }
                }
                // Nobody is drawing this side. Ordinary for a daemon with no
                // window, and the whole explanation for a call that shows the
                // peer and a blank square where this side should be — which
                // is a front end that did not subscribe, not a camera that
                // failed. Said once; it does not change mid-call.
                Delivery::NoSubscriber => {
                    if !std::mem::replace(&mut unwatched, true) {
                        debug!("the self-view has nobody drawing it; local video is not published");
                    }
                }
                // The window's queue is full and the frame is gone. Said
                // once, because the alternative is what this call cost: a
                // self-view black from the first frame to the last, with
                // every hop upstream of it reporting success and this one
                // reporting nothing at all.
                Delivery::Dropped => {
                    dropped_by_the_window += 1;
                    if dropped_by_the_window == 1 {
                        warn!(
                            "the window's video queue refused a frame of the self-view; \
                             it is not keeping up"
                        );
                    }
                }
            }
            gap = delivery.still_owes_a_gap(gap);
        }
        let handed_over = plane.try_send(data);
        if handed_over.is_ok() {
            to_the_plane += 1;
            if to_the_plane == 1 {
                // The last hop this side owns. Past here the frame is the
                // library's to packetise and send, so a call whose peer draws
                // nothing while this line is present is a fault downstream of
                // us — which is the distinction the log could not make.
                debug!("the first frame of local video was handed to the media plane");
            }
            if let Some(audit) = idr_audit {
                debug!("local video IDR NALs: {audit}");
            }
        } else {
            refused_by_the_plane += 1;
            if refused_by_the_plane == 1 {
                // The peer sees nothing from here if this is every frame. The
                // media plane draining slower than the camera fills it is the
                // one hop between an encoder that works and a call with no
                // outbound picture, and it had no line of its own.
                warn!("the media plane refused a frame of local video; asking for a keyframe");
            }
            // The plane is behind. Whatever it sends next has to be
            // decodable on its own, since everything after a gap references
            // a unit the peer never received.
            request_keyframe();
        }
    }
    let call_id = read(&call_id);
    debug!(
        "local video for {call_id} ended ({taken} frame(s) taken, {to_the_plane} handed to the \
         media plane, {refused_by_the_plane} refused there, {drawn} handed to the window, \
         {dropped_by_the_window} refused there)"
    );
    // Every way out, including the `break` above: the flag says this pump
    // produces no more, and a caller wiring the camera up reads it before the
    // report — which is what the registry is torn down by, and which finds
    // nothing while the camera is not in it yet.
    alive.store(false, Ordering::Relaxed);
    frames.close();
    if !stopping.load(Ordering::Relaxed) {
        warn!("local video on call {call_id} ended unexpectedly; releasing its camera");
        lost(call_id, camera_id);
    }
}

/// The peer's picture, on its way to whoever draws it.
async fn pump_remote(
    call_id: CallIdSlot,
    frames: async_channel::Receiver<VideoFrame>,
    publisher: VideoPublisher,
    picture_lost: PictureLost,
    stopping: Arc<AtomicBool>,
) {
    // Runs for as long as the call does, whoever is or is not watching. A
    // pump that stopped at the first frame nobody took would leave the peer's
    // picture gone for good the moment a window closed and reopened.
    let mut gap = false;
    // The counters the local pump has had all along, and this one never did.
    // A remote pane that stays black is the same silence here whether the
    // library delivered nothing, the window refused everything, or the peer
    // simply never sent a keyframe — three unrelated faults, and the log
    // could not tell them apart because this half reported only that it had
    // ended.
    let mut taken = 0u64;
    let mut keyframes = 0u64;
    let mut drawn = 0u64;
    let mut dropped_by_the_window = 0u64;
    let mut unwatched = false;
    while let Ok(frame) = frames.recv().await {
        if stopping.load(Ordering::Relaxed) {
            break;
        }
        taken += 1;
        let keyframe = frame.keyframe;
        if keyframe {
            keyframes += 1;
        }
        if taken == 1 {
            let bytes = frame.data.len();
            let kind = if keyframe {
                "keyframe"
            } else {
                "not a keyframe"
            };
            debug!(
                "the first access unit of the peer's video reached the session ({bytes} bytes, {kind})"
            );
        }
        let delivery = publish(&publisher, || {
            CallVideoFrame::new(
                read(&call_id),
                VideoStream::Remote,
                frame.data,
                keyframe,
                frame.orientation,
            )
            .after_a_gap(gap)
        });
        match delivery {
            Delivery::Sent => {
                drawn += 1;
                if drawn == 1 {
                    debug!("the first access unit of the peer's video was handed to the window");
                }
            }
            Delivery::NoSubscriber => {
                if !std::mem::replace(&mut unwatched, true) {
                    debug!("the peer's video has nobody drawing it; it is not published");
                }
            }
            // The expensive one, and the reason it gets a warning rather than
            // a counter alone: a unit lost here is one the *peer* has to
            // replace. This is the only place that knows it happened -- the
            // library handed the unit over intact, so nothing below sees a
            // loss -- which is why the ask is made from here and not left to
            // the peer's own periodic keyframe.
            //
            // Asked on every dropped unit rather than the first: a window that
            // sheds sheds a run, and coalescing a run into one request is what
            // the engine's throttle is for.
            Delivery::Dropped => {
                dropped_by_the_window += 1;
                if dropped_by_the_window == 1 {
                    warn!(
                        "the window's video queue refused the peer's picture; asking the peer for \
                         the keyframe that ends the gap"
                    );
                }
                picture_lost(&read(&call_id));
            }
        }
        // Beside the request above rather than instead of it: the ask takes a
        // round trip and the frames in between still reference what is gone.
        gap = delivery.still_owes_a_gap(gap);
    }
    debug!(
        "remote video for {} ended ({taken} unit(s) taken, {keyframes} keyframe(s), {drawn} \
         handed to the window, {dropped_by_the_window} refused there)",
        read(&call_id)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every IDR that goes out names the NALs a peer decoder needs in the
    /// log, so a keyframe without parameter sets is visible rather than a
    /// silent non-starter on the far side.
    #[cfg_attr(not(target_family = "wasm"), test)]
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    fn idr_nal_audit_names_parameter_sets_and_slices() {
        let annexb = |types: &[u8]| {
            let mut data = Vec::new();
            for nal_type in types {
                data.extend_from_slice(&[0, 0, 0, 1, 0x60 | nal_type]);
            }
            data
        };
        assert_eq!(idr_nal_types(&annexb(&[7, 8, 5, 5])), vec![7, 8, 5, 5]);
        assert_eq!(idr_nal_types(&annexb(&[5])), vec![5]);
        // No start codes: AVCC length words, not Annex-B — reported empty
        // rather than misread, since the pipeline downstream splits on
        // start codes and would yield nothing either.
        assert!(idr_nal_types(&[0, 0, 0, 12, 0x65, 1, 2, 3]).is_empty());
        assert!(idr_nal_types(&[]).is_empty());
        // The sets come out hex without start codes, first of each kind
        // wins, and a unit without them says none rather than guessing.
        let mut unit = vec![0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f];
        unit.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x06, 0xe2]);
        unit.extend_from_slice(&[0, 0, 0, 1, 0x65, 1, 2, 3]);
        assert_eq!(
            idr_parameter_sets(&unit),
            ("6742c01f".to_string(), "68ce06e2".to_string())
        );
        assert_eq!(
            idr_parameter_sets(&annexb(&[5])),
            ("none".to_string(), "none".to_string())
        );
    }

    /// A camera we asked to stop is not a camera that was lost.
    ///
    /// The two arrive at the pump as the same thing — a closed frame channel —
    /// because closing that channel is how `LocalVideo::stop` ends a task a
    /// page cannot abort. Without the flag this reported `CameraLost` on every
    /// deliberate teardown, and the registry tore down a call that was already
    /// on its way out. The plane's receiver is deliberately kept alive here:
    /// that is what makes `plane.is_closed()` false and leaves the flag as the
    /// only thing that can tell the difference.
    #[cfg_attr(not(target_family = "wasm"), tokio::test)]
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    async fn a_camera_we_stopped_is_not_a_camera_that_was_lost() {
        let (frames_tx, frames) = async_channel::bounded::<EncodedFrame>(1);
        let (plane, _plane_rx) = async_channel::bounded(1);
        let reported = Arc::new(AtomicBool::new(false));
        let stopping = Arc::new(AtomicBool::new(false));

        let lost: CameraLost = {
            let reported = reported.clone();
            Arc::new(move |_, _| reported.store(true, Ordering::Relaxed))
        };
        let pump = LocalPump {
            call_id: slot("call-1"),
            frames,
            request_keyframe: || {},
            plane,
            publisher: VideoPublisher {
                sender: Arc::new(std::sync::Mutex::new(None)),
                watched: Arc::new(AtomicBool::new(false)),
            },
            lost,
            camera_id: 1,
            live: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(true)),
            stopping: stopping.clone(),
        };

        // What `LocalVideo::stop` does, in the order it does it.
        stopping.store(true, Ordering::Relaxed);
        frames_tx.close();
        pump_local(pump).await;

        assert!(
            !reported.load(Ordering::Relaxed),
            "stopping the camera ourselves must not report it as lost"
        );
    }

    /// A window that could not keep up is the one loss nothing below this can
    /// see: the library handed the access unit over intact, so the peer is
    /// never told, and its decoder waits on a reference we threw away.
    ///
    /// Asked on every dropped unit rather than the first, because a window
    /// that sheds sheds a run and coalescing a run into one request is the
    /// engine's job, not this pump's.
    #[tokio::test]
    async fn a_picture_the_window_refused_asks_the_peer_to_start_over() {
        let (frames_tx, frames) = async_channel::bounded::<VideoFrame>(4);
        // One slot, filled and never read: every unit after the first is
        // refused, which is what a window falling behind looks like here.
        let (sender, _held) = tokio::sync::mpsc::channel(1);
        let publisher = VideoPublisher {
            sender: Arc::new(std::sync::Mutex::new(Some(sender))),
            watched: Arc::new(AtomicBool::new(true)),
        };

        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let picture_lost: PictureLost = {
            let asked = asked.clone();
            Arc::new(move |call_id: &str| {
                asked
                    .lock()
                    .expect("asked poisoned")
                    .push(call_id.to_string())
            })
        };

        for _ in 0..3 {
            frames_tx
                .try_send(VideoFrame::new(vec![0, 0, 0, 1, 0x65]))
                .expect("the pump has not read yet");
        }
        frames_tx.close();

        pump_remote(
            slot("call-1"),
            frames,
            publisher,
            picture_lost,
            Arc::new(AtomicBool::new(false)),
        )
        .await;

        let asked = asked.lock().expect("asked poisoned");
        assert_eq!(
            *asked,
            vec!["call-1".to_string(), "call-1".to_string()],
            "the first frame fills the queue; the two it refused each ask"
        );
    }

    /// The call's media plane going away ends the pump too, and the camera it
    /// belongs to used to go on reading as live: a caller wiring it into the
    /// registry kept it, so the device stayed open with its light on and
    /// every window drew a direction nothing would ever arrive on.
    #[cfg_attr(not(target_family = "wasm"), tokio::test)]
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    async fn a_camera_whose_plane_closed_stops_reading_as_live() {
        let (frames_tx, frames) = async_channel::bounded(1);
        let (plane, plane_rx) = async_channel::bounded(1);
        let alive = Arc::new(AtomicBool::new(true));
        let reported = Arc::new(AtomicBool::new(false));

        let lost: CameraLost = {
            let reported = reported.clone();
            Arc::new(move |_, _| reported.store(true, Ordering::Relaxed))
        };
        let pump = LocalPump {
            call_id: slot("call-1"),
            frames,
            request_keyframe: || {},
            plane,
            publisher: VideoPublisher {
                sender: Arc::new(std::sync::Mutex::new(None)),
                watched: Arc::new(AtomicBool::new(false)),
            },
            lost,
            camera_id: 1,
            live: Arc::new(AtomicBool::new(false)),
            alive: alive.clone(),
            stopping: Arc::new(AtomicBool::new(false)),
        };

        // The call ends: the plane's receiver goes, and the next frame the
        // camera produces has nowhere to be sent.
        drop(plane_rx);
        frames_tx
            .send(EncodedFrame {
                data: vec![0u8; 4],
                keyframe: true,
            })
            .await
            .expect("the pump is still reading");
        pump_local(pump).await;

        assert!(
            !alive.load(Ordering::Relaxed),
            "the pump has stopped, so the camera may not read as producing"
        );
        assert!(
            reported.load(Ordering::Relaxed),
            "an endpoint ending without a user stop must release its registered camera"
        );
    }

    #[cfg_attr(not(target_family = "wasm"), tokio::test)]
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    async fn an_idle_camera_loses_its_endpoint_without_waiting_for_a_frame() {
        let (frames_tx, frames) = async_channel::bounded(1);
        let (plane, plane_rx) = async_channel::bounded(1);
        let alive = Arc::new(AtomicBool::new(true));
        let call_id = slot("pending-call");
        let reported = Arc::new(std::sync::Mutex::new(Vec::new()));
        let lost: CameraLost = {
            let alive = alive.clone();
            let call_id = call_id.clone();
            let reported = reported.clone();
            let frames_tx = frames_tx.clone();
            Arc::new(move |id, camera_id| {
                assert!(!alive.load(Ordering::Relaxed));
                assert!(frames_tx.is_closed());
                *call_id
                    .try_lock()
                    .expect("callback must not hold the id lock") = "replacement-call".to_string();
                reported.lock().unwrap().push((id, camera_id));
            })
        };
        let pump = LocalPump {
            call_id: call_id.clone(),
            frames,
            request_keyframe: || panic!("an idle camera needs no keyframe"),
            plane,
            publisher: VideoPublisher {
                sender: Arc::new(std::sync::Mutex::new(None)),
                watched: Arc::new(AtomicBool::new(false)),
            },
            lost,
            camera_id: 42,
            live: Arc::new(AtomicBool::new(false)),
            alive,
            stopping: Arc::new(AtomicBool::new(false)),
        };
        let mut pumping = Box::pin(pump_local(pump));
        assert!(
            futures_lite::future::poll_once(&mut pumping)
                .await
                .is_none()
        );
        *call_id.lock().unwrap() = "assigned-call".to_string();
        drop(plane_rx);
        assert!(
            futures_lite::future::poll_once(&mut pumping)
                .await
                .is_some()
        );
        assert_eq!(
            *reported.lock().unwrap(),
            vec![("assigned-call".to_string(), 42)]
        );
    }

    #[cfg_attr(not(target_family = "wasm"), tokio::test)]
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    async fn deliberate_stop_discards_queued_frames_even_with_a_closed_endpoint() {
        for endpoint_closed in [false, true] {
            let (frames_tx, frames) = async_channel::bounded(1);
            frames_tx
                .try_send(EncodedFrame {
                    data: vec![0],
                    keyframe: true,
                })
                .unwrap();
            let (plane, plane_rx) = async_channel::bounded(1);
            if endpoint_closed {
                plane_rx.close();
            }
            let (sender, mut published) = tokio::sync::mpsc::channel(1);
            let stopping = Arc::new(AtomicBool::new(true));
            frames.close();
            pump_local(LocalPump {
                call_id: slot("stopped-call"),
                frames,
                request_keyframe: || panic!("a stopped camera needs no keyframe"),
                plane,
                publisher: VideoPublisher {
                    sender: Arc::new(std::sync::Mutex::new(Some(sender))),
                    watched: Arc::new(AtomicBool::new(true)),
                },
                lost: Arc::new(|_, _| panic!("user stop is not camera loss")),
                camera_id: 7,
                live: Arc::new(AtomicBool::new(true)),
                alive: Arc::new(AtomicBool::new(true)),
                stopping,
            })
            .await;
            assert!(published.try_recv().is_err());
            assert!(plane_rx.try_recv().is_err());
        }
    }

    #[cfg_attr(not(target_family = "wasm"), tokio::test)]
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    async fn dropping_or_stopping_the_owner_retires_both_pumps() {
        for explicit_stop in [false, true] {
            let (frames_tx, frames) = async_channel::bounded(1);
            let (plane, _plane_rx) = async_channel::bounded(1);
            let (sink_tx, sink) = async_channel::bounded(1);
            let (sender, mut published) = tokio::sync::mpsc::channel(2);
            let publisher = VideoPublisher {
                sender: Arc::new(std::sync::Mutex::new(Some(sender))),
                watched: Arc::new(AtomicBool::new(true)),
            };
            let live = Arc::new(AtomicBool::new(true));
            let alive = Arc::new(AtomicBool::new(true));
            let stopping = Arc::new(AtomicBool::new(false));
            let id = slot("retired-call");
            let (done_tx, done) = async_channel::bounded(2);
            let local = LocalPump {
                call_id: id.clone(),
                frames: frames.clone(),
                request_keyframe: || panic!("a retired camera needs no keyframe"),
                plane: plane.clone(),
                publisher: publisher.clone(),
                lost: Arc::new(|_, _| panic!("owner retirement is not camera loss")),
                camera_id: 9,
                live: live.clone(),
                alive: alive.clone(),
                stopping: stopping.clone(),
            };
            let pump = {
                let done_tx = done_tx.clone();
                crate::exec::spawn(async move {
                    pump_local(local).await;
                    done_tx.try_send(()).unwrap();
                })
            };
            let remote_pump = {
                let id = id.clone();
                let sink = sink.clone();
                let stopping = stopping.clone();
                crate::exec::spawn(async move {
                    pump_remote(id, sink, publisher, Arc::new(|_| {}), stopping).await;
                    done_tx.try_send(()).unwrap();
                })
            };
            let owner = LocalVideo {
                camera: None,
                live,
                pump: Some(pump),
                frames,
                stopping,
                remote_pump: Some(remote_pump),
                sink,
                alive: alive.clone(),
                id,
                camera_id: 9,
                plane,
            };
            frames_tx
                .try_send(EncodedFrame {
                    data: vec![0],
                    keyframe: true,
                })
                .unwrap();
            sink_tx.try_send(VideoFrame::new(vec![0])).unwrap();
            if explicit_stop {
                owner.stop().await;
            } else {
                drop(owner);
            }
            done.recv().await.unwrap();
            done.recv().await.unwrap();
            assert!(frames_tx.is_closed());
            assert!(sink_tx.is_closed());
            assert!(!alive.load(Ordering::Relaxed));
            assert!(published.try_recv().is_err());
        }
    }

    #[cfg_attr(not(target_family = "wasm"), tokio::test)]
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    async fn capture_failure_still_reports_its_camera_generation() {
        let (frames_tx, frames) = async_channel::bounded(1);
        let (plane, _plane_rx) = async_channel::bounded(1);
        let reported = Arc::new(std::sync::Mutex::new(Vec::new()));
        let lost: CameraLost = {
            let reported = reported.clone();
            Arc::new(move |id, camera_id| reported.lock().unwrap().push((id, camera_id)))
        };
        drop(frames_tx);
        pump_local(LocalPump {
            call_id: slot("failed-capture"),
            frames,
            request_keyframe: || {},
            plane,
            publisher: VideoPublisher {
                sender: Arc::new(std::sync::Mutex::new(None)),
                watched: Arc::new(AtomicBool::new(false)),
            },
            lost,
            camera_id: 31,
            live: Arc::new(AtomicBool::new(true)),
            alive: Arc::new(AtomicBool::new(true)),
            stopping: Arc::new(AtomicBool::new(false)),
        })
        .await;
        assert_eq!(
            *reported.lock().unwrap(),
            vec![("failed-capture".to_string(), 31)]
        );
    }

    /// A gap is spent by the frame that carries it, and nothing else. A slot
    /// nobody was listening to used to clear one: a frame dropped, the next
    /// found the subscriber replaced, and the one after that arrived unmarked
    /// at a decoder still missing the units before it.
    #[test]
    fn a_gap_survives_a_frame_nobody_was_there_to_take() {
        let pending = Delivery::Dropped.still_owes_a_gap(false);
        assert!(pending, "a drop is what owes a gap");

        let across = Delivery::NoSubscriber.still_owes_a_gap(pending);
        assert!(across, "and nobody watching does not settle it");

        assert!(
            !Delivery::Sent.still_owes_a_gap(across),
            "the frame that arrives is what spends it"
        );
        assert!(
            !Delivery::NoSubscriber.still_owes_a_gap(false),
            "and nothing owed stays nothing owed"
        );
    }
}

#[cfg(all(test, target_family = "wasm"))]
#[path = "camera_lifecycle_tests.rs"]
mod camera_lifecycle_tests;
