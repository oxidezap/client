//! One camera, opened and encoding, for as long as a call holds it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::{Result, anyhow, bail};
use log::{debug, warn};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

use crate::EncodedFrame;
use crate::VideoQuality;

/// H.264 Constrained Baseline, Level 3.1 — the profile a WhatsApp video call
/// carries, and the one [`VideoQuality::checked`] bounds its numbers by.
const CODEC: &str = "avc1.42e01f";

/// How many encoded units may wait for the session.
///
/// Two, like the desktop's plane depth, and for the same reason: an access
/// unit that cannot be delivered now is worth nothing later, and a queue here
/// is latency the person on screen can see.
const FRAME_DEPTH: usize = 2;

/// The microsecond clock `VideoFrame` timestamps count in.
const MICROS_PER_SECOND: f64 = 1_000_000.0;

/// Whether this browser has a camera and an H.264 encoder at all.
///
/// Asked before a call is offered as a video call rather than after, the same
/// way `oxidezap_audio::can_record` is asked before the microphone is drawn:
/// the alternative is a person granting camera permission to a call that then
/// cannot encode.
///
/// Cheap and synchronous, so it answers on the presence of the two APIs
/// rather than on a codec support query — `VideoEncoder.isConfigSupported` is
/// a promise, and a browser that has `VideoEncoder` and refuses
/// `avc1.42e01f` fails at [`open_camera`] with the reason.
#[must_use]
pub fn is_available() -> bool {
    let global = js_sys::global();
    let defined = |name: &str| {
        js_sys::Reflect::get(&global, &wasm_bindgen::JsValue::from_str(name))
            .is_ok_and(|v| !v.is_undefined() && !v.is_null())
    };
    defined("VideoEncoder")
        && web_sys::window().is_some_and(|w| w.navigator().media_devices().is_ok())
}

/// What the pump reads between frames.
struct Control {
    /// Set by [`CameraControl::request_keyframe`], spent by the next tick.
    ///
    /// A flag rather than a message, because the only question the encoder
    /// asks is "is the next frame an IDR" and two requests before one frame
    /// are one IDR.
    keyframe: Cell<bool>,
}

/// The handle a call keeps to ask for a keyframe.
///
/// Cloneable and `!Send`, unlike the desktop's: a page has one thread and the
/// objects behind this are the browser's, none of which crosses one.
#[derive(Clone)]
pub struct CameraControl(Rc<Control>);

impl CameraControl {
    /// Make the next frame decodable on its own.
    ///
    /// Asked whenever something downstream lost a unit: everything after a
    /// gap references a picture the far side never received.
    pub fn request_keyframe(&self) {
        self.0.keyframe.set(true);
    }
}

/// Stops a camera that was opened but never handed to [`Held`].
///
/// Dropping a `MediaStream` does **not** stop its tracks: the specification
/// ties a source's lifetime to `stop()` on each track, not to the object, so
/// a stream that only ever gets garbage collected leaves the camera running
/// with its indicator lit. Every `?` between `getUserMedia` returning and
/// `Held` taking ownership is such a path — an encoder the browser will not
/// build, a codec it refuses, a timer that will not start — and each of them
/// downgrades the call to voice, which is exactly when the light must go out.
struct CameraGuard(Option<web_sys::MediaStream>);

impl CameraGuard {
    /// Hand the stream on; setup succeeded, so [`Held`] closes it from here.
    fn release(mut self) -> web_sys::MediaStream {
        self.0.take().expect("a guard is released once")
    }
}

impl Drop for CameraGuard {
    fn drop(&mut self) {
        if let Some(stream) = self.0.take() {
            stop_tracks(&stream);
            debug!("the camera is closed again: it opened but its setup did not finish");
        }
    }
}

/// End every track, which is what actually releases the device.
fn stop_tracks(stream: &web_sys::MediaStream) {
    for track in stream.get_tracks().iter() {
        if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
            track.stop();
        }
    }
}

/// Everything one open camera keeps alive, released together.
///
/// The closures are held because the browser calls into them; the element is
/// held because a `<video>` removed from the document stops producing frames.
struct Held {
    stream: web_sys::MediaStream,
    element: web_sys::HtmlVideoElement,
    encoder: web_sys::VideoEncoder,
    timer: Option<i32>,
    _on_tick: Closure<dyn FnMut()>,
    _on_chunk: Closure<dyn FnMut(web_sys::EncodedVideoChunk, wasm_bindgen::JsValue)>,
    _on_error: Closure<dyn FnMut(wasm_bindgen::JsValue)>,
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(timer) = self.timer.take()
            && let Some(window) = web_sys::window()
        {
            window.clear_interval_with_handle(timer);
        }
        // `close` rather than `flush`: a call that has ended has no use for
        // the units still in the encoder, and flushing would deliver them to
        // a channel nobody is reading.
        if self.encoder.state() != web_sys::CodecState::Closed {
            let _ = self.encoder.close();
        }
        self.element.set_src_object(None);
        stop_tracks(&self.stream);
        debug!("the camera is closed");
    }
}

/// One opened camera, feeding encoded access units to a channel.
///
/// The same three questions the desktop's answers — its frames, its quality,
/// its control — so the session's video plane is written once.
pub struct CameraStream {
    frames: async_channel::Receiver<EncodedFrame>,
    quality: VideoQuality,
    control: CameraControl,
    /// `Option` so [`Self::stop`] can take it; `None` only during teardown.
    held: Option<Held>,
}

impl CameraStream {
    /// Encoded access units, in capture order.
    #[must_use]
    pub fn frames(&self) -> async_channel::Receiver<EncodedFrame> {
        self.frames.clone()
    }

    /// What the camera was opened at, which is what paces RTP.
    #[must_use]
    pub fn quality(&self) -> VideoQuality {
        self.quality
    }

    /// The handle to ask for a keyframe with.
    #[must_use]
    pub fn control(&self) -> CameraControl {
        self.control.clone()
    }

    /// Close the device.
    ///
    /// Async to match the desktop's, which waits for the capture thread to
    /// let go of the device because the next call opens the same one. Nothing
    /// to wait for here — releasing a `MediaStreamTrack` is synchronous — but
    /// one signature is what keeps the session's teardown written once.
    #[allow(clippy::unused_async)]
    pub async fn stop(mut self) {
        drop(self.held.take());
    }
}

/// Open the camera and start encoding at `quality`.
///
/// Async where the desktop's is blocking, and it has to be: `getUserMedia` is
/// a permission prompt. Both are awaited *before* an offer or an accept goes
/// out, so a device that will not open downgrades the call to voice rather
/// than leaving a video call with no picture in it.
///
/// # Errors
///
/// If the browser has no encoder, if the camera is refused or busy, or if the
/// encoder will not take Constrained Baseline at these numbers.
pub async fn open_camera(quality: VideoQuality) -> Result<CameraStream> {
    if !is_available() {
        bail!("this browser has no VideoEncoder, so it cannot send a picture");
    }
    let window = web_sys::window().ok_or_else(|| anyhow!("no window to open a camera from"))?;
    // Armed before anything else can fail: from here to `Held` the camera is
    // open, and every `?` below would otherwise leave it that way.
    let guard = CameraGuard(Some(open_device(&window, quality).await?));
    let stream = guard.0.as_ref().expect("just armed").clone();
    let element = attach(&window, &stream)?;

    let (tx, rx) = async_channel::bounded::<EncodedFrame>(FRAME_DEPTH);
    // Before the callback rather than after, so the chunk handler can ask for
    // a keyframe when it is the one that drops a unit.
    let control = CameraControl(Rc::new(Control {
        keyframe: Cell::new(true),
    }));
    let on_chunk = {
        let tx = tx.clone();
        let control = control.clone();
        Closure::<dyn FnMut(web_sys::EncodedVideoChunk, wasm_bindgen::JsValue)>::new(
            move |chunk: web_sys::EncodedVideoChunk, _metadata: wasm_bindgen::JsValue| {
                let mut data = vec![0u8; chunk.byte_length() as usize];
                // A copy that failed leaves zeros, which is a NAL nothing can
                // parse; dropped instead, and the gap is what asks for the
                // next keyframe further down the line.
                if chunk.copy_to_with_u8_slice(&mut data).is_err() {
                    warn!("an encoded video chunk could not be read");
                    return;
                }
                let keyframe = chunk.type_() == web_sys::EncodedVideoChunkType::Key;
                // Dropped rather than queued: this is the same trade the
                // desktop's plane makes one step further along. A unit the
                // session has not taken by the time the next is encoded is
                // one the peer is better off not waiting for.
                //
                // The keyframe is asked for *here*, though, and not left to
                // the session: a unit dropped at this queue never reaches the
                // session at all, so its own "my send failed" path cannot see
                // the gap, and every P-frame after this one references a
                // picture the peer will never hold. Only on `Full` — a closed
                // channel means the call is over and nothing wants a picture.
                if let Err(async_channel::TrySendError::Full(_)) =
                    tx.try_send(EncodedFrame { data, keyframe })
                {
                    control.request_keyframe();
                }
            },
        )
    };
    let on_error = {
        let tx = tx.clone();
        Closure::<dyn FnMut(wasm_bindgen::JsValue)>::new(move |error: wasm_bindgen::JsValue| {
            // The encoder is done after an error, and this is asynchronous:
            // nothing above is returning an `Err` for it. Closing the channel
            // is what the session reads as the device having gone, so it has
            // to happen *here* — the chunk callback holds a sender for as
            // long as the camera is held, so without this the frame pump
            // waits forever on a channel nothing will ever send to again, and
            // the registry goes on drawing a camera that stopped.
            warn!("the video encoder stopped: {}", describe(&error));
            tx.close();
        })
    };

    let init = web_sys::VideoEncoderInit::new(
        on_error.as_ref().unchecked_ref(),
        on_chunk.as_ref().unchecked_ref(),
    );
    let encoder = web_sys::VideoEncoder::new(&init)
        .map_err(|e| anyhow!("no video encoder: {}", describe(&e)))?;
    encoder.configure(&encoder_config(quality)?).map_err(|e| {
        anyhow!(
            "the encoder refused {CODEC} at these settings: {}",
            describe(&e)
        )
    })?;

    let on_tick = tick(&element, &encoder, &control, quality);
    let timer = window
        .set_interval_with_callback_and_timeout_and_arguments_0(
            on_tick.as_ref().unchecked_ref(),
            i32::try_from(1000 / quality.fps.max(1)).unwrap_or(50),
        )
        .map_err(|e| anyhow!("the capture timer would not start: {}", describe(&e)))?;

    debug!(
        "the browser camera is open at {}x{}@{}",
        quality.width, quality.height, quality.fps
    );
    Ok(CameraStream {
        frames: rx,
        quality,
        control,
        held: Some(Held {
            // Setup finished: the guard hands the camera to `Held`, which is
            // what closes it from here.
            stream: guard.release(),
            element,
            encoder,
            timer: Some(timer),
            _on_tick: on_tick,
            _on_chunk: on_chunk,
            _on_error: on_error,
        }),
    })
}

/// Ask for the camera at the size and rate the call will claim.
///
/// `ideal` rather than `exact`: a device that cannot do 720p20 should give
/// what it has rather than refuse, and the encoder scales what it is handed.
/// An `exact` constraint here is a call that fails on a webcam.
async fn open_device(
    window: &web_sys::Window,
    quality: VideoQuality,
) -> Result<web_sys::MediaStream> {
    let devices = window
        .navigator()
        .media_devices()
        .map_err(|e| anyhow!("this browser offers no camera: {}", describe(&e)))?;
    let video = js_sys::Object::new();
    for (name, value) in [
        ("width", f64::from(quality.width)),
        ("height", f64::from(quality.height)),
        ("frameRate", f64::from(quality.fps)),
    ] {
        let ideal = js_sys::Object::new();
        let _ = js_sys::Reflect::set(
            &ideal,
            &wasm_bindgen::JsValue::from_str("ideal"),
            &wasm_bindgen::JsValue::from_f64(value),
        );
        let _ = js_sys::Reflect::set(&video, &wasm_bindgen::JsValue::from_str(name), &ideal);
    }
    let constraints = web_sys::MediaStreamConstraints::new();
    constraints.set_video(&video);

    wasm_bindgen_futures::JsFuture::from(
        devices
            .get_user_media_with_constraints(&constraints)
            .map_err(|e| anyhow!("the camera could not be opened: {}", describe(&e)))?,
    )
    .await
    .map_err(|e| anyhow!("the camera was refused: {}", describe(&e)))?
    .dyn_into::<web_sys::MediaStream>()
    .map_err(|_| anyhow!("the browser opened something that is not a stream"))
}

/// A `<video>` playing the stream, so a `VideoFrame` can be taken from it.
///
/// Muted and `playsinline`, and never added to the document: an element with
/// no parent still decodes, and one that were added would draw the self-view
/// twice — once here and once wherever the front end puts the decoded frames.
/// Muted also matters on its own, since autoplay of an unmuted element is
/// refused.
fn attach(
    window: &web_sys::Window,
    stream: &web_sys::MediaStream,
) -> Result<web_sys::HtmlVideoElement> {
    let document = window
        .document()
        .ok_or_else(|| anyhow!("no document to attach a camera to"))?;
    let element = document
        .create_element("video")
        .map_err(|e| anyhow!("no video element: {}", describe(&e)))?
        .dyn_into::<web_sys::HtmlVideoElement>()
        .map_err(|_| anyhow!("the browser made something that is not a video element"))?;
    element.set_muted(true);
    let _ = element.set_attribute("playsinline", "");
    element.set_src_object(Some(stream));
    // The promise is deliberately not awaited: playback starting is what the
    // first frames wait on anyway, and a rejected autoplay on a muted element
    // with a live `srcObject` is not something a call should stop for.
    let _ = element.play();
    Ok(element)
}

/// One capture tick: take what the element is showing and encode it.
fn tick(
    element: &web_sys::HtmlVideoElement,
    encoder: &web_sys::VideoEncoder,
    control: &CameraControl,
    quality: VideoQuality,
) -> Closure<dyn FnMut()> {
    let element = element.clone();
    let encoder = encoder.clone();
    let control = control.clone();
    // Counted rather than read off a clock: the timestamps have to advance by
    // exactly the stride the call negotiated, or the peer's playout drifts
    // against its own RTP timestamps. A wall clock would deliver the jitter
    // of the page's timer into the stream.
    let frames = Rc::new(Cell::new(0u64));
    let step = MICROS_PER_SECOND / f64::from(quality.fps.max(1));
    let complained = Rc::new(RefCell::new(false));
    Closure::<dyn FnMut()>::new(move || {
        if encoder.state() != web_sys::CodecState::Configured {
            return;
        }
        // A queue that is growing means the encoder is behind the timer, and
        // handing it more is how a page turns a slow machine into an
        // unbounded backlog. Skipping is what the desktop's bounded channel
        // does one step later.
        if encoder.encode_queue_size() > 2 {
            return;
        }
        if element.ready_state() < 2 || element.video_width() == 0 {
            // Nothing is playing yet: the element is still opening the
            // stream. Not an error, and not worth a frame of duplicated
            // black.
            return;
        }
        let init = web_sys::VideoFrameInit::new();
        // Through `Reflect` rather than `set_timestamp`, which web-sys types
        // as `i32` where the specification says `long long`. Microseconds in
        // an `i32` wrap after about thirty-five minutes, and a call that long
        // would hand the encoder a timestamp that went backwards.
        let _ = js_sys::Reflect::set(
            &init,
            &wasm_bindgen::JsValue::from_str("timestamp"),
            &wasm_bindgen::JsValue::from_f64(frames.get() as f64 * step),
        );
        let frame = match web_sys::VideoFrame::new_with_html_video_element_and_video_frame_init(
            &element, &init,
        ) {
            Ok(frame) => frame,
            Err(e) => {
                if !std::mem::replace(&mut complained.borrow_mut(), true) {
                    warn!("a camera frame could not be taken: {}", describe(&e));
                }
                return;
            }
        };
        let options = web_sys::VideoEncoderEncodeOptions::new();
        options.set_key_frame(control.0.keyframe.replace(false));
        if let Err(e) = encoder.encode_with_options(&frame, &options)
            && !std::mem::replace(&mut complained.borrow_mut(), true)
        {
            warn!("a camera frame could not be encoded: {}", describe(&e));
        }
        // Closed explicitly: a `VideoFrame` holds a decoder-side buffer that
        // garbage collection does not release in time, and a page that leaks
        // them stops capturing within seconds.
        frame.close();
        frames.set(frames.get().saturating_add(1));
    })
}

/// Constrained Baseline, in the shape the library's video source reads.
fn encoder_config(quality: VideoQuality) -> Result<web_sys::VideoEncoderConfig> {
    let config = web_sys::VideoEncoderConfig::new(CODEC, quality.height, quality.width);
    config.set_width(quality.width);
    config.set_height(quality.height);
    config.set_bitrate(quality.bitrate_kbps.saturating_mul(1000));
    config.set_framerate(f64::from(quality.fps));
    // A call is latency before quality: `realtime` is what stops the encoder
    // buffering frames to spend its bitrate more evenly.
    config.set_latency_mode(web_sys::LatencyMode::Realtime);
    // The one field with no setter, and the one that decides whether anything
    // downstream can read the output at all: without it the chunks are AVCC
    // with the parameter sets in the metadata, and the library's source wants
    // Annex-B with them in front of every IDR.
    let avc = js_sys::Object::new();
    js_sys::Reflect::set(
        &avc,
        &wasm_bindgen::JsValue::from_str("format"),
        &wasm_bindgen::JsValue::from_str("annexb"),
    )
    .map_err(|e| anyhow!("avc config: {}", describe(&e)))?;
    js_sys::Reflect::set(&config, &wasm_bindgen::JsValue::from_str("avc"), &avc)
        .map_err(|e| anyhow!("avc config: {}", describe(&e)))?;
    Ok(config)
}

/// A `JsValue` as something worth putting in a log line.
fn describe(value: &wasm_bindgen::JsValue) -> String {
    value
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| value.as_string())
        .unwrap_or_else(|| format!("{value:?}"))
}
