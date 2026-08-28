//! The camera, on its own thread.
//!
//! A capture backend is blocking on every platform — `frame()` waits for the
//! sensor — and an encode is a hundred microseconds of CPU, so both live on
//! one dedicated thread and hand finished access units to the async side
//! through a channel. That is the same shape the microphone takes in
//! `oxidezap-audio`, and for the same reason: a device driven from a runtime
//! task would block a worker for as long as the call lasted.
//!
//! The channel is short and lossy on purpose. Video is the one stream where
//! the newest frame is the only one worth having: a queue that grows is a
//! call that drifts further behind the person talking, and a frame dropped is
//! a frame nobody waits for. What a drop *does* cost is the reference chain,
//! so it also asks the encoder for a keyframe — otherwise every frame after
//! the gap points at one the peer never received, and the picture stays
//! broken until the periodic one comes round.

use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use nokhwa::Camera;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
    Resolution,
};
use portable_atomic::{AtomicBool, Ordering};
use wacore::time::Instant;

use crate::convert::{Frames, I420Buffer};
use crate::encoder::{EncodedFrame, H264Encoder};
use crate::{VideoQuality, format_name};

/// How many finished access units may wait for the call's media plane.
///
/// Two: one being sent and one ready. A third would only ever be watched
/// going stale.
const QUEUE_DEPTH: usize = 2;

/// A running camera: the frames it produces, and the two things a caller can
/// say to it.
pub struct CameraStream {
    frames: async_channel::Receiver<EncodedFrame>,
    control: CameraControl,
    /// Kept so a caller can wait for the device to be released. Dropping the
    /// stream asks the thread to stop; joining is what proves it has.
    thread: Option<std::thread::JoinHandle<()>>,
    quality: VideoQuality,
}

/// The two things a caller can say to a running camera, detached from the
/// camera itself.
///
/// A handle rather than an `Arc<CameraStream>`, because the stream is what
/// *owns* the capture thread: a second owner would mean closing the device is
/// a matter of dropping the last reference, and nothing could then wait for
/// the thread to let go of it. This is the half that can be shared with a
/// task freely.
#[derive(Clone, Default)]
pub struct CameraControl(Arc<Control>);

impl CameraControl {
    /// Ask for the next frame to be a keyframe.
    ///
    /// The peer says so through RTCP when it has lost the stream, and the
    /// only useful answer is an IDR — resending the same P-frames it could
    /// not decode is not one.
    pub fn request_keyframe(&self) {
        self.0.keyframe.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Control {
    stop: AtomicBool,
    keyframe: AtomicBool,
}

impl CameraStream {
    /// The encoded frames.
    ///
    /// Whole units with their keyframe flag rather than bare bytes, because
    /// this stream has two readers: the call's media plane, which wants the
    /// payload, and whoever draws the self-view, which needs to know where it
    /// may start decoding. Recomputing the flag by re-walking the unit is the
    /// alternative, and the encoder already knows.
    pub fn frames(&self) -> async_channel::Receiver<EncodedFrame> {
        self.frames.clone()
    }

    /// What the stream is actually producing, which is not always what was
    /// asked for: a camera with no 720p mode is opened at the closest one it
    /// has.
    pub fn quality(&self) -> VideoQuality {
        self.quality
    }

    /// The shareable half: what a task driving the call may say to the
    /// device without owning it.
    pub fn control(&self) -> CameraControl {
        self.control.clone()
    }

    /// Stop the camera and wait for the device to be closed.
    ///
    /// Waited for rather than left to `Drop`: the next call opens the same
    /// device, and a capture backend that still holds it fails the open
    /// rather than queuing behind it. The wait is bounded by one frame — the
    /// thread is asleep in `frame()` and notices on the way out.
    pub fn stop(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        self.control.0.stop.store(true, Ordering::Relaxed);
        // The producer selects on nothing: closing the channel is what stops
        // it blocking on a send to a receiver that is going away.
        self.frames.close();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            log::warn!("the camera thread ended badly");
        }
    }
}

impl Drop for CameraStream {
    fn drop(&mut self) {
        self.shut_down();
    }
}

/// Open the camera and start encoding.
///
/// Returns once the device is open and configured, so a caller that is about
/// to answer a video call learns *here* that there is no camera — rather than
/// accepting one and then having nothing to send.
pub fn open(quality: VideoQuality) -> Result<CameraStream> {
    authorize()?;

    let (frames_tx, frames_rx) = async_channel::bounded(QUEUE_DEPTH);
    let control = CameraControl::default();
    // The open happens on the capture thread — some backends bind the device
    // to the thread that opened it — so the result comes back over a
    // rendezvous rather than being returned directly.
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    let thread = {
        let control = Arc::clone(&control.0);
        std::thread::Builder::new()
            .name("oxidezap-camera".to_string())
            .spawn(move || run(quality, &control, &frames_tx, &ready_tx))
            .context("spawning the camera thread")?
    };

    match ready_rx.recv() {
        Ok(Ok(quality)) => Ok(CameraStream {
            frames: frames_rx,
            control,
            thread: Some(thread),
            quality,
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(anyhow!("the camera thread ended before opening a device"))
        }
    }
}

/// Ask for camera access, and wait for the answer.
///
/// macOS will not hand out a camera until the user has been asked, and the
/// ask is asynchronous: the prompt is raised and the result arrives on a
/// callback. Continuing straight into the open runs it while permission is
/// still undetermined, which fails — so a video call placed the first time
/// the app ever used the camera was downgraded to voice, moments before the
/// user granted it. Blocking is safe here: the only caller runs [`open`] on a
/// thread of its own.
///
/// Bounded, because the answer is a person: a prompt nobody dismisses must
/// not hold a call's setup open forever.
#[cfg(target_os = "macos")]
fn authorize() -> Result<()> {
    use anyhow::bail;

    if nokhwa::nokhwa_check() {
        return Ok(());
    }
    // A rendezvous rather than a plain channel: the callback must be `Sync`,
    // which `SyncSender` is and `Sender` is not.
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    nokhwa::nokhwa_initialize(move |granted| {
        let _ = tx.send(granted);
    });
    match rx.recv_timeout(AUTHORIZATION_TIMEOUT) {
        Ok(true) => Ok(()),
        Ok(false) => bail!("camera access was refused"),
        Err(_) => bail!("camera access was not answered within {AUTHORIZATION_TIMEOUT:?}"),
    }
}

/// Every other platform answers at open time, through the open itself.
#[cfg(not(target_os = "macos"))]
fn authorize() -> Result<()> {
    Ok(())
}

/// How long to wait for somebody to answer the permission prompt.
#[cfg(target_os = "macos")]
const AUTHORIZATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

type Ready = std::sync::mpsc::SyncSender<Result<VideoQuality>>;

fn run(
    quality: VideoQuality,
    control: &Control,
    frames: &async_channel::Sender<EncodedFrame>,
    ready: &Ready,
) {
    let opened = match start(quality) {
        Ok(opened) => opened,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let Opened {
        mut camera,
        mut converter,
        mut encoder,
        quality,
    } = opened;
    if ready.send(Ok(quality)).is_err() {
        // Nobody is waiting for this camera any more.
        let _ = camera.stop_stream();
        return;
    }

    let started = Instant::now();
    while !control.stop.load(Ordering::Relaxed) && !frames.is_closed() {
        let buffer = match camera.frame() {
            Ok(buffer) => buffer,
            Err(e) => {
                // A camera that has been unplugged reports this forever, and
                // there is no recovery a call can do: the direction stops and
                // the audio carries on.
                log::error!("camera capture failed: {e}");
                break;
            }
        };
        if control.keyframe.swap(false, Ordering::Relaxed) {
            encoder.request_keyframe();
        }
        let at = openh264::Timestamp::from_millis(started.elapsed().as_millis() as u64);
        let encoded = match convert_and_encode(&buffer, &mut converter, &mut encoder, at) {
            Ok(Some(frame)) => frame,
            // The rate control skipped it, which is the encoder doing its job.
            Ok(None) => continue,
            Err(e) => {
                log::warn!("dropping a camera frame: {e}");
                continue;
            }
        };
        if frames.try_send(encoded).is_err() {
            // Either the media plane is behind or the call is over. The next
            // frame the peer *does* get has to be one it can decode on its
            // own, since everything after a gap references what it missed.
            control.keyframe.store(true, Ordering::Relaxed);
        }
    }

    if let Err(e) = camera.stop_stream() {
        log::warn!("the camera did not close cleanly: {e}");
    }
    log::debug!("camera stopped");
}

struct Opened {
    camera: Camera,
    converter: Frames,
    encoder: H264Encoder,
    quality: VideoQuality,
}

/// A mode every webcam has had for twenty years, for a camera whose closest
/// match to what was asked for is one a peer could not decode.
const FALLBACK: VideoQuality = VideoQuality {
    width: 640,
    height: 480,
    fps: 15,
    bitrate_kbps: 600,
};

/// Open the camera at something a video call may actually offer.
///
/// The mode a backend picks is *near* what was asked for, not what was asked
/// for: a camera with no 720p20 answers with whatever it does have, and that
/// can be 1080p — past the Level 3.1 a call is bounded by — or a frame rate
/// that does not divide the RTP clock, which would silently truncate the
/// timestamp stride and drift the stream against its own timestamps. So the
/// mode that came back is checked like any other input, and a camera that
/// cannot meet it is asked again for something every device has.
fn start(wanted: VideoQuality) -> Result<Opened> {
    match start_at(wanted) {
        Ok(opened) => Ok(opened),
        Err(first) => {
            log::warn!(
                "reopening the camera at {}x{}: {first:#}",
                FALLBACK.width,
                FALLBACK.height
            );
            start_at(VideoQuality {
                bitrate_kbps: wanted.bitrate_kbps.min(FALLBACK.bitrate_kbps),
                ..FALLBACK
            })
            .map_err(|second| second.context(format!("{first:#}")))
        }
    }
}

/// What the device settled on, refused if a call could not carry it.
///
/// Separate from the open so every rejection takes the same way out: the mode
/// a backend picks is *near* what was asked for, not what was asked for, and
/// discovering that is a reason to close the device rather than to leave it
/// streaming into nothing.
fn prepare(camera: &Camera, wanted: VideoQuality) -> Result<(Frames, VideoQuality)> {
    let format = camera.camera_format();
    let (width, height) = (
        format.resolution().width() as usize,
        format.resolution().height() as usize,
    );
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        bail!("produces {width}x{height}, which 4:2:0 cannot carry");
    }
    let converter = match format.format() {
        FrameFormat::YUYV | FrameFormat::NV12 | FrameFormat::GRAY => Frames::planar(width, height)?,
        _ => Frames::rgb(width, height)?,
    };
    // Held to the same bounds the requested numbers were: this is what the
    // encoder is configured with and what the RTP stride is derived from, and
    // neither is a place to discover that a camera answered with 1080p.
    let quality = VideoQuality {
        width: format.resolution().width(),
        height: format.resolution().height(),
        fps: format.frame_rate(),
        bitrate_kbps: wanted.bitrate_kbps,
    }
    .checked()
    .context("opened at a mode a call cannot carry")?;
    Ok((converter, quality))
}

fn start_at(wanted: VideoQuality) -> Result<Opened> {
    let index = configured_device();
    let requested =
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(CameraFormat::new(
            Resolution::new(wanted.width, wanted.height),
            // The format the closest-match search starts from. A camera that
            // has no MJPEG mode is matched on the rest and answers in
            // whatever it does have, which is what the converter branches on.
            FrameFormat::MJPEG,
            wanted.fps,
        )));
    let mut camera =
        Camera::new(index.clone(), requested).with_context(|| format!("opening camera {index}"))?;
    camera
        .open_stream()
        .with_context(|| format!("starting the stream on camera {index}"))?;

    // Every way out of the checks below closes the stream on the way, because
    // the one caller that sees a failure reopens the same device immediately
    // and a backend still holding it fails that open rather than queueing
    // behind it.
    let prepared = prepare(&camera, wanted).inspect_err(|_| {
        if let Err(e) = camera.stop_stream() {
            log::warn!("the camera did not close cleanly after a refused mode: {e}");
        }
    });
    let (converter, quality) = prepared.with_context(|| format!("camera {index}"))?;
    let format = camera.camera_format();
    let encoder = H264Encoder::new(quality)?;
    log::info!(
        "camera {index} open: {}x{} @ {} fps, {} in, H.264 out at {} kbps",
        quality.width,
        quality.height,
        quality.fps,
        format_name(format.format()),
        quality.bitrate_kbps,
    );
    Ok(Opened {
        camera,
        converter,
        encoder,
        quality,
    })
}

fn convert_and_encode(
    buffer: &nokhwa::Buffer,
    converter: &mut Frames,
    encoder: &mut H264Encoder,
    at: openh264::Timestamp,
) -> Result<Option<EncodedFrame>> {
    match converter {
        Frames::Planar(planes) => {
            read_planar(buffer, planes)?;
            encoder.encode(&planes.as_source(), at)
        }
        Frames::Rgb { rgb, yuv } => {
            buffer
                .decode_image_to_buffer::<RgbFormat>(rgb)
                .map_err(|e| {
                    anyhow!(
                        "decoding a {} frame: {e}",
                        format_name(buffer.source_frame_format())
                    )
                })?;
            let (width, height) = openh264::formats::YUVSource::dimensions(yuv);
            yuv.read_rgb8(openh264::formats::RgbSliceU8::new(rgb, (width, height)));
            encoder.encode(yuv, at)
        }
    }
}

fn read_planar(buffer: &nokhwa::Buffer, planes: &mut I420Buffer) -> Result<()> {
    match buffer.source_frame_format() {
        FrameFormat::YUYV => planes.read_yuyv(buffer.buffer()),
        FrameFormat::NV12 => planes.read_nv12(buffer.buffer()),
        FrameFormat::GRAY => planes.read_gray(buffer.buffer()),
        other => bail!("camera changed to {} mid-stream", format_name(other)),
    }
}

/// Which camera to open.
///
/// The first one unless told otherwise, because a machine with one camera is
/// the case and asking would be a setting nobody would ever change. The
/// override takes an index or a backend-specific name, both of which the
/// capture backends accept.
pub(crate) fn configured_device() -> CameraIndex {
    match std::env::var("OXIDEZAP_CAMERA") {
        Ok(value) if !value.trim().is_empty() => match value.trim().parse::<u32>() {
            Ok(index) => CameraIndex::Index(index),
            Err(_) => CameraIndex::String(value.trim().to_string()),
        },
        _ => CameraIndex::Index(0),
    }
}

/// Whether this platform has a capture backend at all, without opening a
/// device.
///
/// A caller offering a video call wants to know before it offers one, and
/// enumerating is much cheaper than opening — though not free, which is why
/// nothing calls it per frame.
pub fn is_available() -> bool {
    match nokhwa::query(ApiBackend::Auto) {
        Ok(cameras) => !cameras.is_empty(),
        Err(e) => {
            log::debug!("no camera backend: {e}");
            false
        }
    }
}
