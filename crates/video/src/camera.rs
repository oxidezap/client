//! The camera, on its own thread.
//!
//! A capture backend is blocking on every platform — `frame()` waits for the
//! sensor, while encoding runs on the CPU, so both live on
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
//!
//! `RUST_LOG=info,oxidezap_video::camera=debug` enables stage totals and frame
//! counters at frame boundaries after five seconds, plus a final partial window.
//! These measure elapsed wall time, not CPU time. Capture includes blocking
//! sensor wait; even conversion and encoding include time spent descheduled.
//! Use a native CPU profile alongside these logs to identify CPU-heavy functions.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use nokhwa::Camera;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, CameraInfo, FrameFormat, RequestedFormat,
    RequestedFormatType,
};
use portable_atomic::{AtomicBool, Ordering};
use wacore::time::Instant;

use crate::EncodedFrame;
use crate::convert::{Frames, I420Buffer};
use crate::encoder::H264Encoder;
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

    /// Stop the camera, from an async caller.
    ///
    /// The signature the browser backend has to have, so the session's video
    /// plane has one teardown rather than two. The blocking half is
    /// [`Self::stop_now`], and it is where the wait actually happens.
    pub async fn stop(self) {
        // A capture thread asleep in `frame()` is exactly the thing that must
        // not be waited on from a runtime thread.
        if let Err(e) = tokio::task::spawn_blocking(move || self.stop_now()).await {
            log::warn!("closing the camera failed: {e}");
        }
    }

    /// Stop the camera and wait for the device to be closed.
    ///
    /// Waited for rather than left to `Drop`: the next call opens the same
    /// device, and a capture backend that still holds it fails the open
    /// rather than queuing behind it. The wait is bounded by one frame — the
    /// thread is asleep in `frame()` and notices on the way out.
    pub fn stop_now(mut self) {
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
pub fn open(quality: VideoQuality, device: Option<String>) -> Result<CameraStream> {
    let quality = quality.checked()?;
    let index = device
        .map(|id| parse_device(&id))
        .transpose()?
        .unwrap_or_else(configured_device);
    // After the authorization, not before: on macOS nothing else in the
    // library may run until `nokhwa_initialize` has answered, so an
    // inventory read first can report an empty backend over cameras that
    // are right there.
    authorize()?;
    // Read against the device list from here on: the open names only the
    // camera it took, and never what else was there — a virtual camera
    // sitting at index 0 while the real one waits at 1, a privacy-blocked
    // device, an empty backend. Enumerating is much cheaper than opening,
    // and opens happen at call rate.
    log::info!("cameras in reach: {}", inventory());

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
            .spawn(move || run(quality, &index, &control, &frames_tx, &ready_tx))
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
    index: &CameraIndex,
    control: &Control,
    frames: &async_channel::Sender<EncodedFrame>,
    ready: &Ready,
) {
    log::debug!(
        "requesting native camera {}x{} @ {} fps, {} kbps",
        quality.width,
        quality.height,
        quality.fps,
        quality.bitrate_kbps,
    );
    let opened = match start(quality, index) {
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
    let mut failures = Failures::default();
    let mut timing: Option<CameraTiming> = None;
    while !control.stop.load(Ordering::Relaxed) && !frames.is_closed() {
        if log::log_enabled!(log::Level::Debug) {
            let now = Instant::now();
            let timing = timing.get_or_insert_with(|| CameraTiming::new(now));
            if let Some(window) = timing.take_if_due(now) {
                window.report(now);
            }
        } else {
            timing = None;
        }
        let buffer = match timed(timing.as_mut().map(|t| &mut t.capture), || camera.frame()) {
            Ok(buffer) => buffer,
            Err(e) => {
                if let Some(timing) = &mut timing {
                    timing.record(FrameOutcome::CaptureFailed);
                }
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
        let encoded =
            match convert_and_encode(&buffer, &mut converter, &mut encoder, at, timing.as_mut()) {
                Ok(Some(frame)) => {
                    failures.worked();
                    frame
                }
                // The rate control skipped it, which is the encoder doing its job.
                Ok(None) => {
                    if let Some(timing) = &mut timing {
                        timing.record(FrameOutcome::Skipped);
                    }
                    failures.worked();
                    continue;
                }
                Err(e) => {
                    if let Some(timing) = &mut timing {
                        timing.record(FrameOutcome::ProcessingFailed);
                    }
                    log::warn!("dropping a camera frame: {e}");
                    // Some of these are one frame — a truncated buffer, a decode
                    // that fell over — and some are permanent: a camera that
                    // changes format mid-stream fails every frame from then on.
                    // Retrying the permanent ones forever holds the channel open,
                    // so nothing reports the loss and every window goes on saying
                    // video is live in front of a pane no picture will reach.
                    if failures.gave_up() {
                        log::error!("the camera stopped producing frames this side can encode");
                        break;
                    }
                    continue;
                }
            };
        let keyframe = encoded.keyframe;
        let dropped = frames.try_send(encoded).is_err();
        if let Some(timing) = &mut timing {
            timing.record(FrameOutcome::Encoded { keyframe, dropped });
        }
        if dropped {
            // Either the media plane is behind or the call is over. The next
            // frame the peer *does* get has to be one it can decode on its
            // own, since everything after a gap references what it missed.
            control.keyframe.store(true, Ordering::Relaxed);
        }
    }

    if let Some(timing) = timing {
        timing.report(Instant::now());
    }
    if let Err(e) = camera.stop_stream() {
        log::warn!("the camera did not close cleanly: {e}");
    }
    log::debug!("camera stopped");
}

const TIMING_INTERVAL: Duration = Duration::from_secs(5);

enum FrameOutcome {
    CaptureFailed,
    ProcessingFailed,
    Skipped,
    Encoded { keyframe: bool, dropped: bool },
}

struct CameraTiming {
    since: Instant,
    capture: Duration,
    conversion: Duration,
    encode: Duration,
    captured: u64,
    capture_errors: u64,
    processing_errors: u64,
    encoded: u64,
    keyframes: u64,
    skipped: u64,
    dropped: u64,
}

impl CameraTiming {
    fn new(since: Instant) -> Self {
        Self {
            since,
            capture: Duration::ZERO,
            conversion: Duration::ZERO,
            encode: Duration::ZERO,
            captured: 0,
            capture_errors: 0,
            processing_errors: 0,
            encoded: 0,
            keyframes: 0,
            skipped: 0,
            dropped: 0,
        }
    }

    fn record(&mut self, outcome: FrameOutcome) {
        match outcome {
            FrameOutcome::CaptureFailed => {
                self.capture_errors += 1;
                return;
            }
            FrameOutcome::ProcessingFailed => self.processing_errors += 1,
            FrameOutcome::Skipped => self.skipped += 1,
            FrameOutcome::Encoded { keyframe, dropped } => {
                self.encoded += 1;
                self.keyframes += u64::from(keyframe);
                self.dropped += u64::from(dropped);
            }
        }
        self.captured += 1;
    }

    fn take_if_due(&mut self, now: Instant) -> Option<Self> {
        (now - self.since >= TIMING_INTERVAL).then(|| std::mem::replace(self, Self::new(now)))
    }

    fn report(&self, now: Instant) {
        let seconds = (now - self.since).as_secs_f64();
        if seconds == 0.0 || self.captured + self.capture_errors == 0 {
            return;
        }
        log::debug!(
            "native camera timing over {seconds:.3}s, elapsed wall time not CPU; \
             capture includes blocking wait: captured={} observed_fps={:.2} encoded={} \
             keyframes={} skipped={} processing_errors={} capture_errors={} queue_drops={} \
             capture_total_ms={:.3} decode_conversion_total_ms={:.3} encode_total_ms={:.3}",
            self.captured,
            self.captured as f64 / seconds,
            self.encoded,
            self.keyframes,
            self.skipped,
            self.processing_errors,
            self.capture_errors,
            self.dropped,
            self.capture.as_secs_f64() * 1000.0,
            self.conversion.as_secs_f64() * 1000.0,
            self.encode.as_secs_f64() * 1000.0,
        );
    }
}

// Disabled diagnostics read no clocks. Enabled diagnostics use two reads per
// stage and one window check per frame, with no per-frame formatting or storage.
fn timed<T>(elapsed: Option<&mut Duration>, work: impl FnOnce() -> T) -> T {
    let started = elapsed.as_ref().map(|_| Instant::now());
    let result = work();
    if let (Some(elapsed), Some(started)) = (elapsed, started) {
        *elapsed += started.elapsed();
    }
    result
}

/// How many frames in a row may fail to convert before the camera is treated
/// as gone.
///
/// A second of them at any sane rate. Long enough that a run of bad buffers
/// is ridden out, short enough that a person watching a dead pane is not.
const FAILURES_BEFORE_LOST: u32 = 30;

/// A run of consecutive frame failures, and whether it has gone on too long.
///
/// A run rather than a total, because the two failures are different things:
/// a camera that drops one frame an hour is working, and one that has failed
/// every frame since it changed format is not going to start again.
#[derive(Default)]
struct Failures {
    run: u32,
}

impl Failures {
    /// A frame made it through, so whatever went wrong before was transient.
    fn worked(&mut self) {
        self.run = 0;
    }

    /// Count a failure, and say whether this camera should be given up on.
    fn gave_up(&mut self) -> bool {
        self.run += 1;
        self.run >= FAILURES_BEFORE_LOST
    }
}

struct Opened {
    camera: Camera,
    converter: Frames,
    encoder: H264Encoder,
    quality: VideoQuality,
}

// Order breaks ties between otherwise equivalent modes. MJPEG preserves the
// existing preference, but is not a requirement on cameras exposing raw video.
const INPUT_FORMATS: &[FrameFormat] = &[
    FrameFormat::MJPEG,
    FrameFormat::YUYV,
    FrameFormat::NV12,
    FrameFormat::GRAY,
    FrameFormat::RAWRGB,
    FrameFormat::RAWBGR,
];

trait CaptureDevice {
    fn formats(&mut self) -> Result<Vec<CameraFormat>>;
    fn start_stream(&mut self) -> Result<()>;
    fn stop_stream(&mut self) -> Result<()>;
}

impl CaptureDevice for Camera {
    fn formats(&mut self) -> Result<Vec<CameraFormat>> {
        advertised_formats::read(self)
    }
    fn start_stream(&mut self) -> Result<()> {
        Ok(self.open_stream()?)
    }
    fn stop_stream(&mut self) -> Result<()> {
        Ok(Camera::stop_stream(self)?)
    }
}

// Nokhwa 0.10.11's AVFoundation compatible_list_by_resolution filters with
// `!= fourcc`. Its aggregate API therefore loses or relabels native tuples.
// Use the same direct format source as its constructor on that platform.
#[cfg(target_os = "macos")]
mod advertised_formats {
    use super::*;
    pub fn read(camera: &mut Camera) -> Result<Vec<CameraFormat>> {
        let device = nokhwa_bindings_macos::AVCaptureDevice::new(camera.index())?;
        Ok(device.supported_formats()?)
    }
}
#[cfg(not(target_os = "macos"))]
mod advertised_formats {
    use super::*;
    use nokhwa::utils::Resolution;
    use std::collections::HashMap;

    pub fn read(camera: &mut Camera) -> Result<Vec<CameraFormat>> {
        // Media Foundation's compatible_fourcc stops after two formats.
        // Ask for every converter input instead of using that aggregate API.
        collect(|input| Ok(camera.compatible_list_by_resolution(input)?))
    }

    fn collect(
        mut read: impl FnMut(FrameFormat) -> Result<HashMap<Resolution, Vec<u32>>>,
    ) -> Result<Vec<CameraFormat>> {
        let mut modes = Vec::new();
        let mut errors = Vec::new();
        for input in INPUT_FORMATS {
            match read(*input) {
                Ok(resolutions) => {
                    for (resolution, rates) in resolutions {
                        modes.extend(
                            rates
                                .into_iter()
                                .map(|fps| CameraFormat::new(resolution, *input, fps)),
                        );
                    }
                }
                // Unsupported pixel formats may be errors on some backends.
                Err(error) => errors.push(format!("{input}: {error:#}")),
            }
        }
        if errors.len() == INPUT_FORMATS.len() {
            bail!("listing camera formats failed: {}", errors.join("; "));
        }
        Ok(modes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_third_pixel_format_remains_available_after_two_unusable_formats() {
            let advertised = [
                CameraFormat::new(Resolution::new(1280, 720), FrameFormat::MJPEG, 60),
                CameraFormat::new(Resolution::new(1920, 1080), FrameFormat::YUYV, 30),
                CameraFormat::new(Resolution::new(640, 480), FrameFormat::NV12, 30),
            ];
            let wanted = VideoQuality::default();
            assert!(
                candidates(advertised[..2].to_vec(), wanted).is_empty(),
                "the legacy two-format aggregate hides the only usable input"
            );
            let mut queried = Vec::new();
            let complete = collect(|input| {
                queried.push(input);
                let mut resolutions: HashMap<Resolution, Vec<u32>> = HashMap::new();
                for mode in advertised.iter().filter(|mode| mode.format() == input) {
                    resolutions
                        .entry(mode.resolution())
                        .or_default()
                        .push(mode.frame_rate());
                }
                if resolutions.is_empty() {
                    bail!("fake unsupported pixel format");
                }
                Ok(resolutions)
            })
            .unwrap();
            assert_eq!(queried, INPUT_FORMATS);
            assert_eq!(candidates(complete, wanted), vec![advertised[2]]);
        }
    }
}

fn mode_quality(format: CameraFormat, wanted: VideoQuality) -> Result<VideoQuality> {
    VideoQuality {
        width: format.resolution().width(),
        height: format.resolution().height(),
        fps: format.frame_rate(),
        bitrate_kbps: wanted.bitrate_kbps,
    }
    .checked()
}

fn candidates(formats: Vec<CameraFormat>, wanted: VideoQuality) -> Vec<CameraFormat> {
    let mut formats: Vec<_> = formats
        .into_iter()
        .filter(|format| {
            INPUT_FORMATS.contains(&format.format()) && mode_quality(*format, wanted).is_ok()
        })
        .collect();
    formats.sort_by_key(|format| {
        let res = format.resolution();
        (
            u64::from(res.width().abs_diff(wanted.width)).pow(2)
                + u64::from(res.height().abs_diff(wanted.height)).pow(2),
            format.frame_rate().abs_diff(wanted.fps),
            INPUT_FORMATS
                .iter()
                .position(|input| *input == format.format()),
            *format,
        )
    });
    formats.dedup();
    formats
}

// Discovery opens no stream and retains no device as a cache. Each failed
// attempt is stopped and dropped before the factory is called again.
fn negotiate<C: CaptureDevice, T>(
    wanted: VideoQuality,
    mut open: impl FnMut(RequestedFormatType, &[FrameFormat]) -> Result<C>,
    mut prepare: impl FnMut(&C) -> Result<T>,
) -> Result<(C, T)> {
    let mut errors = Vec::new();
    let mut available = None;
    for input in INPUT_FORMATS {
        match open(RequestedFormatType::None, &[*input]) {
            Ok(mut device) => {
                let formats = device.formats();
                let closed = device.stop_stream();
                drop(device);
                closed.context("closing camera after mode discovery")?;
                match formats {
                    Ok(formats) => {
                        available = Some(formats);
                        break;
                    }
                    Err(error) => errors.push(format!("listing modes: {error:#}")),
                }
            }
            Err(error) => errors.push(format!("discovering {input}: {error:#}")),
        }
    }
    let available = available.ok_or_else(|| anyhow!("no camera modes: {}", errors.join("; ")))?;
    let modes = candidates(available, wanted);
    if modes.is_empty() {
        bail!("camera advertises no supported mode within video call limits");
    }
    for mode in modes {
        let mut device = match open(RequestedFormatType::Exact(mode), INPUT_FORMATS) {
            Ok(device) => device,
            Err(error) => {
                errors.push(format!("opening {mode}: {error:#}"));
                continue;
            }
        };
        let prepared = device.start_stream().and_then(|()| prepare(&device));
        match prepared {
            Ok(prepared) => return Ok((device, prepared)),
            Err(error) => {
                errors.push(format!("preparing {mode}: {error:#}"));
                let closed = device.stop_stream();
                drop(device);
                closed.context("closing camera before retry")?;
            }
        }
    }
    bail!("camera negotiation failed: {}", errors.join("; "))
}

fn start(wanted: VideoQuality, index: &CameraIndex) -> Result<Opened> {
    let (camera, (converter, encoder, quality)) = negotiate(
        wanted,
        |request, formats| {
            Camera::new(
                index.clone(),
                RequestedFormat::with_formats(request, formats),
            )
            .with_context(|| format!("opening camera {index}"))
        },
        |camera| {
            let (converter, quality) = prepare(camera, wanted)?;
            let encoder = H264Encoder::new(quality)?;
            Ok((converter, encoder, quality))
        },
    )?;
    log::info!(
        "camera {index} open: {}x{} @ {} fps, {} in, H.264 at {} kbps; requested {}x{} @ {} fps",
        quality.width,
        quality.height,
        quality.fps,
        format_name(camera.camera_format().format()),
        quality.bitrate_kbps,
        wanted.width,
        wanted.height,
        wanted.fps,
    );
    Ok(Opened {
        camera,
        converter,
        encoder,
        quality,
    })
}

/// What the device settled on, refused if a call could not carry it.
///
/// Separate from the open so every rejection takes the same way out: the mode
/// a backend picks is *near* what was asked for, not what was asked for, and
/// discovering that is a reason to close the device rather than to leave it
/// streaming into nothing.
fn prepare(camera: &Camera, wanted: VideoQuality) -> Result<(Frames, VideoQuality)> {
    let format = camera.camera_format();
    if !INPUT_FORMATS.contains(&format.format()) {
        bail!(
            "camera selected unsupported {}",
            format_name(format.format())
        );
    }
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

fn convert_and_encode(
    buffer: &nokhwa::Buffer,
    converter: &mut Frames,
    encoder: &mut H264Encoder,
    at: openh264::Timestamp,
    mut timing: Option<&mut CameraTiming>,
) -> Result<Option<EncodedFrame>> {
    timed(
        timing.as_mut().map(|t| &mut t.conversion),
        || match converter {
            Frames::Planar(planes) => read_planar(buffer, planes),
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
                Ok(())
            }
        },
    )?;
    timed(timing.map(|t| &mut t.encode), || match converter {
        Frames::Planar(planes) => encoder.encode(&planes.as_source(), at),
        Frames::Rgb { yuv, .. } => encoder.encode(yuv, at),
    })
}

fn read_planar(buffer: &nokhwa::Buffer, planes: &mut I420Buffer) -> Result<()> {
    match buffer.source_frame_format() {
        FrameFormat::YUYV => planes.read_yuyv(buffer.buffer()),
        FrameFormat::NV12 => planes.read_nv12(buffer.buffer()),
        FrameFormat::GRAY => planes.read_gray(buffer.buffer()),
        other => bail!("camera changed to {} mid-stream", format_name(other)),
    }
}

fn parse_device(id: &str) -> Result<CameraIndex> {
    if let Some(index) = id.strip_prefix("index:") {
        return Ok(CameraIndex::Index(
            index.parse().context("invalid camera index")?,
        ));
    }
    if let Some(name) = id.strip_prefix("name:").filter(|name| !name.is_empty()) {
        return Ok(CameraIndex::String(name.to_string()));
    }
    bail!("unknown camera device ID")
}

// Media Foundation resolves a symbolic link on every open, so unplugging a
// camera cannot redirect a saved choice to the device now occupying its index.
#[cfg(target_os = "windows")]
mod device_identity {
    use super::*;
    pub fn index(info: &CameraInfo) -> CameraIndex {
        if info.misc().is_empty() {
            info.index().clone()
        } else {
            CameraIndex::String(info.misc())
        }
    }
}
#[cfg(not(target_os = "windows"))]
mod device_identity {
    use super::*;
    pub fn index(info: &CameraInfo) -> CameraIndex {
        info.index().clone()
    }
}

pub fn enumerate() -> Result<Vec<crate::CameraDevice>> {
    enumerate_with(
        authorize,
        || nokhwa::query(ApiBackend::Auto).context("enumerating cameras"),
        probe_formats,
    )
}

fn probe_formats(index: &CameraIndex) -> Result<Vec<CameraFormat>> {
    let mut errors = Vec::new();
    for input in INPUT_FORMATS {
        let inputs = [*input];
        let request = RequestedFormat::with_formats(RequestedFormatType::None, &inputs);
        match Camera::new(index.clone(), request) {
            Ok(mut camera) => {
                let formats = CaptureDevice::formats(&mut camera);
                let closed = camera.stop_stream();
                drop(camera);
                closed.context("closing camera inventory probe")?;
                return formats;
            }
            Err(error) => errors.push(error.to_string()),
        }
    }
    bail!("camera modes unavailable: {}", errors.join("; "))
}

fn enumerate_with(
    initialize: impl FnOnce() -> Result<()>,
    query: impl FnOnce() -> Result<Vec<CameraInfo>>,
    mut formats: impl FnMut(&CameraIndex) -> Result<Vec<CameraFormat>>,
) -> Result<Vec<crate::CameraDevice>> {
    // AVFoundation requires initialization before even querying the backend.
    // Refused or unanswered access is an error, not an empty inventory.
    initialize()?;
    let devices = query()?;
    let mut result = Vec::with_capacity(devices.len());
    for (position, info) in devices.into_iter().enumerate() {
        let index = device_identity::index(&info);
        let id = match &index {
            CameraIndex::Index(index) => format!("index:{index}"),
            CameraIndex::String(name) => format!("name:{name}"),
        };
        // A busy device still belongs in inventory. Only its modes are missing.
        let available = formats(&index).unwrap_or_else(|error| {
            log::debug!("camera {index} inventory modes unavailable: {error:#}");
            Vec::new()
        });
        let mut modes: Vec<_> = candidates(available, VideoQuality::default())
            .into_iter()
            .map(|format| crate::CameraMode {
                width: format.resolution().width(),
                height: format.resolution().height(),
                fps: format.frame_rate(),
            })
            .collect();
        modes.sort();
        modes.dedup();
        result.push(crate::CameraDevice {
            id,
            label: info.human_name(),
            is_default: position == 0,
            modes,
        });
    }
    Ok(result)
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

/// The devices an open will choose among, as one line.
///
/// Read off enumeration rather than the open because the open names only the
/// device it took. `OXIDEZAP_CAMERA` names the same index and name space
/// [`configured_device`] reads, so the line says exactly what there is to
/// choose from when index 0 is the wrong camera.
fn inventory() -> String {
    match nokhwa::query(ApiBackend::Auto) {
        Ok(cameras) => describe(&cameras),
        Err(e) => format!("unlistable: {e}"),
    }
}

/// The enumeration, in the override's own terms: index, then name.
fn describe(cameras: &[CameraInfo]) -> String {
    if cameras.is_empty() {
        return "none".to_string();
    }
    cameras
        .iter()
        .map(|camera| format!("{:?}: {}", camera.index(), camera.human_name()))
        .collect::<Vec<_>>()
        .join(", ")
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

#[cfg(test)]
mod tests {
    use super::{FAILURES_BEFORE_LOST, Failures};
    use nokhwa::utils::Resolution;

    /// One bad frame is not a broken camera, and neither are many with good
    /// ones between them: what says the device is gone is a run.
    #[test]
    fn a_frame_that_works_ends_the_run() {
        let mut failures = Failures::default();
        for _ in 0..FAILURES_BEFORE_LOST - 1 {
            assert!(!failures.gave_up());
        }
        failures.worked();
        for _ in 0..FAILURES_BEFORE_LOST - 1 {
            assert!(!failures.gave_up());
        }
    }

    /// A camera that changed format mid-stream fails every frame from then
    /// on, and retrying it forever is what keeps a dead direction drawn.
    #[test]
    fn an_unbroken_run_gives_up() {
        let mut failures = Failures::default();
        for _ in 0..FAILURES_BEFORE_LOST - 1 {
            assert!(!failures.gave_up());
        }
        assert!(failures.gave_up());
    }

    /// The inventory reads in the override's own terms, so the line next to
    /// a failed open says what there was to choose from.
    #[test]
    fn the_inventory_names_every_camera_by_index() {
        use nokhwa::utils::{CameraIndex, CameraInfo};

        assert_eq!(super::describe(&[]), "none");
        assert_eq!(
            super::describe(&[
                CameraInfo::new("FaceTime HD Camera", "camera", "", CameraIndex::Index(0)),
                CameraInfo::new("OBS Virtual Camera", "camera", "", CameraIndex::Index(1)),
            ]),
            "Index(0): FaceTime HD Camera, Index(1): OBS Virtual Camera"
        );
    }

    #[test]
    fn timing_windows_aggregate_outcomes_and_reset_at_the_deadline() {
        use super::{CameraTiming, Duration, FrameOutcome, Instant, TIMING_INTERVAL};

        let start = Instant::ZERO;
        let mut timing = CameraTiming::new(start);
        for outcome in [
            FrameOutcome::CaptureFailed,
            FrameOutcome::ProcessingFailed,
            FrameOutcome::Skipped,
            FrameOutcome::Encoded {
                keyframe: true,
                dropped: false,
            },
            FrameOutcome::Encoded {
                keyframe: false,
                dropped: true,
            },
        ] {
            timing.record(outcome);
        }
        timing.capture = Duration::from_millis(4000);
        timing.conversion = Duration::from_millis(200);
        timing.encode = Duration::from_millis(300);
        assert!(
            timing
                .take_if_due(start + TIMING_INTERVAL - Duration::from_nanos(1))
                .is_none()
        );

        let now = start + TIMING_INTERVAL;
        let window = timing.take_if_due(now).expect("five seconds elapsed");
        assert_eq!(window.captured, 4);
        assert_eq!(window.capture_errors, 1);
        assert_eq!(window.processing_errors, 1);
        assert_eq!(window.skipped, 1);
        assert_eq!(window.encoded, 2);
        assert_eq!(window.keyframes, 1);
        assert_eq!(window.dropped, 1);
        assert_eq!(window.capture, Duration::from_millis(4000));
        assert_eq!(window.conversion, Duration::from_millis(200));
        assert_eq!(window.encode, Duration::from_millis(300));

        assert!(timing.take_if_due(now).is_none());
        let next = timing.take_if_due(now + TIMING_INTERVAL).unwrap();
        assert_eq!(next.since, now);
        assert_eq!(
            next.captured + next.capture_errors + next.processing_errors,
            0
        );
        assert_eq!(
            next.encoded + next.keyframes + next.skipped + next.dropped,
            0
        );
        assert_eq!(next.capture + next.conversion + next.encode, Duration::ZERO);
    }

    #[test]
    fn a_delayed_timing_window_does_not_emit_catch_up_reports() {
        use super::{CameraTiming, Duration, FrameOutcome, Instant, TIMING_INTERVAL};

        let mut timing = CameraTiming::new(Instant::ZERO);
        timing.record(FrameOutcome::Skipped);
        let now = Instant::ZERO + Duration::from_secs(60);
        assert_eq!(timing.take_if_due(now).unwrap().skipped, 1);
        assert!(timing.take_if_due(now).is_none());
        assert!(
            timing
                .take_if_due(now + TIMING_INTERVAL - Duration::from_nanos(1))
                .is_none()
        );
    }

    #[test]
    fn disabled_timing_preserves_the_operation_result() {
        let mut calls = 0;
        let result = super::timed(None, || {
            calls += 1;
            Err::<(), _>("synthetic failure")
        });
        assert_eq!(calls, 1);
        assert_eq!(result, Err("synthetic failure"));
    }

    #[test]
    fn timing_does_not_change_encoded_frames_or_encode_failed_conversions() {
        use super::*;

        for format in [FrameFormat::GRAY, FrameFormat::RAWRGB] {
            let mut converter = match format {
                FrameFormat::GRAY => Frames::planar(64, 48),
                _ => Frames::rgb(64, 48),
            }
            .unwrap();
            let mut plain = H264Encoder::new(VideoQuality::default()).unwrap();
            let mut measured = H264Encoder::new(VideoQuality::default()).unwrap();
            let mut timing = CameraTiming::new(Instant::ZERO);
            let short = nokhwa::Buffer::new(Resolution::new(64, 48), &[], FrameFormat::GRAY);
            assert!(
                convert_and_encode(
                    &short,
                    &mut converter,
                    &mut measured,
                    openh264::Timestamp::ZERO,
                    Some(&mut timing),
                )
                .is_err()
            );
            assert_eq!(timing.encode, Duration::ZERO);

            let channels = if format == FrameFormat::GRAY { 1 } else { 3 };
            let buffer = nokhwa::Buffer::new(
                Resolution::new(64, 48),
                &vec![128; 64 * 48 * channels],
                format,
            );
            for millis in [0, 50] {
                let at = openh264::Timestamp::from_millis(millis);
                let expected = convert_and_encode(&buffer, &mut converter, &mut plain, at, None)
                    .unwrap()
                    .unwrap();
                let actual = convert_and_encode(
                    &buffer,
                    &mut converter,
                    &mut measured,
                    at,
                    Some(&mut timing),
                )
                .unwrap()
                .unwrap();
                assert_eq!(actual.data, expected.data);
                assert_eq!(actual.keyframe, expected.keyframe);
            }
        }
    }
}

#[cfg(test)]
mod negotiation_tests {
    use super::*;
    use nokhwa::utils::Resolution;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct State {
        held: bool,
        opens: Vec<CameraFormat>,
        stops: usize,
        drops: usize,
        unclean_drops: usize,
        fail_start: Option<CameraFormat>,
        fail_close: bool,
    }

    struct Fake {
        state: Rc<RefCell<State>>,
        advertised: Vec<CameraFormat>,
        mode: CameraFormat,
        stopped: bool,
    }

    impl CaptureDevice for Fake {
        fn formats(&mut self) -> Result<Vec<CameraFormat>> {
            Ok(self.advertised.clone())
        }
        fn start_stream(&mut self) -> Result<()> {
            if self.state.borrow().fail_start == Some(self.mode) {
                bail!("fake stream refused");
            }
            Ok(())
        }
        fn stop_stream(&mut self) -> Result<()> {
            self.stopped = true;
            let mut state = self.state.borrow_mut();
            state.stops += 1;
            if state.fail_close {
                bail!("fake cleanup failed");
            }
            Ok(())
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let mut state = self.state.borrow_mut();
            state.held = false;
            state.drops += 1;
            state.unclean_drops += usize::from(!self.stopped);
        }
    }

    fn mode(width: u32, height: u32, fps: u32, input: FrameFormat) -> CameraFormat {
        CameraFormat::new(Resolution::new(width, height), input, fps)
    }

    fn factory(
        state: Rc<RefCell<State>>,
        advertised: Vec<CameraFormat>,
    ) -> impl FnMut(RequestedFormatType, &[FrameFormat]) -> Result<Fake> {
        move |request, formats| {
            assert!(
                !state.borrow().held,
                "previous attempt must release the device"
            );
            let mode = RequestedFormat::with_formats(request, formats)
                .fulfill(&advertised)
                .ok_or_else(|| anyhow!("fake unsupported format"))?;
            if matches!(request, RequestedFormatType::Exact(_)) {
                assert!(
                    advertised.contains(&mode),
                    "never invent a resolution/FPS pair"
                );
                state.borrow_mut().opens.push(mode);
            }
            state.borrow_mut().held = true;
            Ok(Fake {
                state: state.clone(),
                advertised: advertised.clone(),
                mode,
                stopped: false,
            })
        }
    }

    #[test]
    fn inventory_initializes_before_query_and_preserves_native_mode_pairs() {
        let stage = std::cell::Cell::new(0);
        let listed = enumerate_with(
            || {
                stage.set(1);
                Ok(())
            },
            || {
                assert_eq!(stage.get(), 1);
                stage.set(2);
                Ok(vec![CameraInfo::new(
                    "Synthetic camera",
                    "",
                    "",
                    CameraIndex::Index(0),
                )])
            },
            |_| {
                assert_eq!(stage.get(), 2);
                // Different pixel formats need not share resolution or FPS.
                Ok(vec![
                    mode(640, 480, 30, FrameFormat::YUYV),
                    mode(1280, 720, 20, FrameFormat::MJPEG),
                ])
            },
        )
        .unwrap();
        assert_eq!(
            listed[0].modes,
            vec![
                crate::CameraMode {
                    width: 640,
                    height: 480,
                    fps: 30
                },
                crate::CameraMode {
                    width: 1280,
                    height: 720,
                    fps: 20
                },
            ]
        );
    }

    #[test]
    fn refused_inventory_access_never_queries_or_probes_devices() {
        let result = enumerate_with(
            || bail!("synthetic access refused"),
            || panic!("query before authorization"),
            |_| panic!("probe before authorization"),
        );
        assert!(result.unwrap_err().to_string().contains("access refused"));
    }

    #[test]
    fn reproduces_both_legacy_mjpeg_attempts_failing_on_a_raw_camera() {
        let advertised = [mode(640, 480, 30, FrameFormat::YUYV)];
        for wanted in [
            VideoQuality::default(),
            VideoQuality {
                width: 640,
                height: 480,
                fps: 15,
                bitrate_kbps: 600,
            },
        ] {
            let requested = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(mode(
                wanted.width,
                wanted.height,
                wanted.fps,
                FrameFormat::MJPEG,
            )));
            assert!(requested.fulfill(&advertised).is_none());
        }
    }

    #[test]
    fn absent_requested_size_does_not_hide_a_usable_advertised_mode() {
        let actual = mode(800, 600, 30, FrameFormat::MJPEG);
        let wanted = VideoQuality::default();
        let legacy = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Closest(mode(
            wanted.width,
            wanted.height,
            wanted.fps,
            FrameFormat::MJPEG,
        )));
        assert!(legacy.fulfill(&[actual]).is_none());
        let state = Rc::new(RefCell::new(State::default()));
        let (mut camera, quality) =
            negotiate(wanted, factory(state.clone(), vec![actual]), |camera| {
                mode_quality(camera.mode, wanted)
            })
            .unwrap();
        assert_eq!((quality.width, quality.height, quality.fps), (800, 600, 30));
        camera.stop_stream().unwrap();
        drop(camera);
        assert_eq!(state.borrow().unclean_drops, 0);
    }

    #[test]
    fn stopping_or_dropping_a_stream_joins_capture_and_allows_reopen() {
        let held = Arc::new(AtomicBool::new(false));
        for explicit in [true, false, true] {
            let (tx, frames) = async_channel::bounded(QUEUE_DEPTH);
            let control = CameraControl::default();
            let running = control.clone();
            let device = held.clone();
            assert!(!device.swap(true, Ordering::SeqCst));
            let thread = std::thread::spawn(move || {
                while !running.0.stop.load(Ordering::Relaxed) && !tx.is_closed() {
                    std::thread::yield_now();
                }
                device.store(false, Ordering::SeqCst);
            });
            let stream = CameraStream {
                frames,
                control,
                thread: Some(thread),
                quality: VideoQuality::default(),
            };
            let receiver = stream.frames();
            if explicit {
                stream.stop_now();
            } else {
                drop(stream);
            }
            assert!(
                !held.load(Ordering::SeqCst),
                "teardown waits for device release"
            );
            assert!(receiver.is_closed());
        }
    }

    #[test]
    fn a_canceled_background_open_drops_its_late_stream_and_releases_capture() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let release_open = Arc::new(std::sync::Barrier::new(2));
        let gate = release_open.clone();
        let (released_tx, released) = std::sync::mpsc::sync_channel(1);
        let opening = runtime.spawn_blocking(move || {
            gate.wait();
            let (tx, frames) = async_channel::bounded(QUEUE_DEPTH);
            let control = CameraControl::default();
            let running = control.clone();
            let thread = std::thread::spawn(move || {
                while !running.0.stop.load(Ordering::Relaxed) && !tx.is_closed() {
                    std::thread::yield_now();
                }
                released_tx.send(()).unwrap();
            });
            CameraStream {
                frames,
                control,
                thread: Some(thread),
                quality: VideoQuality::default(),
            }
        });
        // The async caller canceled while the blocking camera open was still
        // pending. Tokio must drop its eventual output, invoking stream teardown.
        drop(opening);
        release_open.wait();
        released
            .recv_timeout(Duration::from_secs(5))
            .expect("late capture must close");
    }

    #[test]
    fn raw_only_camera_negotiates_without_mjpeg_and_reports_effective_mode() {
        for input in [FrameFormat::YUYV, FrameFormat::NV12, FrameFormat::GRAY] {
            let state = Rc::new(RefCell::new(State::default()));
            let actual = mode(640, 480, 30, input);
            let wanted = VideoQuality::default();
            let (mut camera, quality) =
                negotiate(wanted, factory(state.clone(), vec![actual]), |device| {
                    mode_quality(device.mode, wanted)
                })
                .unwrap();
            assert_eq!((quality.width, quality.height, quality.fps), (640, 480, 30));
            assert_eq!(quality.timestamp_stride(), 3000);
            assert_eq!(state.borrow().opens, vec![actual]);
            camera.stop_stream().unwrap();
            drop(camera);
            assert!(!state.borrow().held);
            assert_eq!(state.borrow().stops, 2);
            assert_eq!(state.borrow().unclean_drops, 0);
        }
    }

    #[test]
    fn stream_and_prepare_failures_release_before_trying_another_advertised_mode() {
        let first = mode(1280, 720, 20, FrameFormat::MJPEG);
        let second = mode(1280, 720, 20, FrameFormat::YUYV);
        let third = mode(640, 480, 15, FrameFormat::NV12);
        let state = Rc::new(RefCell::new(State {
            fail_start: Some(first),
            ..State::default()
        }));
        let (mut camera, quality) = negotiate(
            VideoQuality::default(),
            factory(state.clone(), vec![third, second, first, first]),
            |device| {
                if device.mode == second {
                    bail!("fake encoder failure");
                }
                mode_quality(device.mode, VideoQuality::default())
            },
        )
        .unwrap();
        assert_eq!(state.borrow().opens, vec![first, second, third]);
        assert_eq!(state.borrow().stops, 3); // probe plus two failed attempts
        assert_eq!((quality.width, quality.height, quality.fps), (640, 480, 15));
        camera.stop_stream().unwrap();
        drop(camera);
        assert_eq!(state.borrow().drops, 4);
        assert_eq!(state.borrow().unclean_drops, 0);
    }

    #[test]
    fn filters_invalid_modes_without_synthesizing_a_fallback() {
        let valid = mode(800, 600, 30, FrameFormat::YUYV);
        let advertised = vec![
            mode(1920, 1080, 30, FrameFormat::MJPEG),
            mode(640, 480, 29, FrameFormat::MJPEG),
            mode(641, 480, 30, FrameFormat::NV12),
            mode(0, 480, 30, FrameFormat::GRAY),
            valid,
            valid,
        ];
        assert_eq!(candidates(advertised, VideoQuality::default()), vec![valid]);
    }

    #[test]
    fn all_failures_and_cleanup_failure_leave_no_device_cached() {
        for fail_close in [false, true] {
            let input = mode(640, 480, 30, FrameFormat::NV12);
            let state = Rc::new(RefCell::new(State {
                fail_close,
                ..State::default()
            }));
            let opened = negotiate(
                VideoQuality::default(),
                factory(state.clone(), vec![input]),
                |_| -> Result<()> {
                    bail!("fake device removed");
                },
            );
            assert!(opened.is_err());
            assert!(!state.borrow().held);
            assert_eq!(state.borrow().opens.len(), usize::from(!fail_close));
        }
    }

    #[test]
    fn saved_ids_round_trip_and_do_not_accept_unknown_namespaces() {
        assert_eq!(parse_device("index:3").unwrap(), CameraIndex::Index(3));
        assert_eq!(
            parse_device("name:device-link").unwrap(),
            CameraIndex::String("device-link".into())
        );
        for id in ["3", "name:", "index:invalid", "index:4294967296"] {
            assert!(parse_device(id).is_err());
        }
    }
}
