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

/// The millisecond clock `setInterval` and the deadline count in.
const MILLIS_PER_SECOND: f64 = 1000.0;

/// Whether `VideoEncoder.isConfigSupported` is there to be called.
///
/// Read off the constructor rather than assumed from `VideoEncoder` existing:
/// the static is newer than the interface, and browsers shipped the two apart.
fn has_config_check() -> bool {
    let global = js_sys::global();
    js_sys::Reflect::get(&global, &wasm_bindgen::JsValue::from_str("VideoEncoder"))
        .ok()
        .and_then(|encoder| {
            js_sys::Reflect::get(
                &encoder,
                &wasm_bindgen::JsValue::from_str("isConfigSupported"),
            )
            .ok()
        })
        .is_some_and(|method| method.is_function())
}

/// Whether this browser will really encode what [`encoder_config`] describes.
///
/// `isConfigSupported` is a promise and a static, so it costs one await and
/// no device. A browser without it — the method is newer than `VideoEncoder`
/// itself — answers `true` rather than being refused: the check exists to
/// turn a late failure into an early one, and a browser that cannot be asked
/// is no worse off than before it was.
async fn encoder_supports(config: &web_sys::VideoEncoderConfig) -> bool {
    // Asked for before it is called, because calling an absent static is a
    // synchronous `TypeError` — and web-sys does not bind this one `catch`,
    // so it would take the tab rather than falling through to the `Err` arm
    // below. The one case this helper exists to be lenient about is exactly
    // the one that would have trapped.
    if !has_config_check() {
        debug!("this browser's VideoEncoder has no isConfigSupported; assuming {CODEC} encodes");
        return true;
    }
    let asked = web_sys::VideoEncoder::is_config_supported(config);
    match wasm_bindgen_futures::JsFuture::from(asked).await {
        Ok(answer) => js_sys::Reflect::get(&answer, &wasm_bindgen::JsValue::from_str("supported"))
            .ok()
            .and_then(|v| v.as_bool())
            // Present and false is a refusal; absent is a browser answering
            // in a shape this does not know, which is not a refusal.
            .unwrap_or(true),
        Err(e) => {
            // A throw here is the method not being there, or not liking the
            // config's shape. Neither is an answer about the codec.
            debug!(
                "this browser would not answer isConfigSupported: {}",
                describe(&e)
            );
            true
        }
    }
}

/// Milliseconds on a clock that only goes forward.
///
/// `Date::now` is the wall clock, and the deadline above measures *elapsed*
/// time: an NTP correction backwards would leave `now` under a deadline
/// already met for as long as the adjustment lasted, and every tick would
/// return with the timer and the encoder both healthy. `performance.now` is
/// monotonic by specification. Falls back to the wall clock where there is no
/// `Performance` — a clock that can jump is still better than no frames.
fn monotonic_now() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map_or_else(js_sys::Date::now, |performance| performance.now())
}

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
            // Disarmed before it is stopped. `stop()` is specified not to fire
            // `ended`, but a handler the browser calls after its closure has
            // been dropped is a trap rather than a missed event, and the
            // closures go with the `Held` that is dropping now.
            track.set_onended(None);
            track.stop();
        }
    }
}

/// Closes an encoder that was built but never handed to [`Held`].
///
/// A `VideoEncoder` is asynchronous on both ends — a configuration it will not
/// honour and a runtime failure both arrive at `on_error` later — so an
/// encoder left open after setup returns is one that can still call into
/// closures that have been dropped, which is a trap rather than a leak.
/// Declared after those closures for the reason the audio graph's `Wiring`
/// is: locals drop in reverse, and a guard that ran after them would be
/// closing an encoder whose callbacks were already gone.
struct EncoderGuard(Option<web_sys::VideoEncoder>);

impl EncoderGuard {
    /// Setup succeeded; [`Held`] closes it from here.
    fn release(mut self) -> web_sys::VideoEncoder {
        self.0.take().expect("a guard is released once")
    }
}

impl Drop for EncoderGuard {
    fn drop(&mut self) {
        if let Some(encoder) = self.0.take()
            && encoder.state() != web_sys::CodecState::Closed
        {
            let _ = encoder.close();
            debug!("the video encoder is closed again: it opened but its setup did not finish");
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
    /// One per track; see where it is installed in [`open_camera`].
    _on_ended: Vec<Closure<dyn FnMut(web_sys::Event)>>,
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

    // Asked before the device is, which is the ordering this whole module is
    // built on: a browser that will not encode Constrained Baseline should
    // downgrade the call to voice without ever raising a camera prompt.
    //
    // And asked at all because `configure` is not the answer. It is
    // synchronous and validates the *shape* of the config; an implementation
    // that cannot actually encode these numbers is entitled to say so later,
    // through the error callback — by which time `open_camera` has returned,
    // the offer has gone out as video, and the recovery is a call that
    // downgrades itself after signalling rather than before.
    let config = encoder_config(quality)?;
    if !encoder_supports(&config).await {
        bail!("this browser's VideoEncoder will not encode {CODEC} at these settings");
    }

    // Armed before anything else can fail: from here to `Held` the camera is
    // open, and every `?` below would otherwise leave it that way.
    let guard = CameraGuard(Some(open_device(&window, quality).await?));
    let stream = guard.0.as_ref().expect("just armed").clone();
    let element = attach(&window, &stream).await?;

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
                    // The encoder produced this unit and counted it: whatever
                    // it emits next references a picture that is about to go
                    // nowhere. The same answer the full-queue branch gives,
                    // for the same reason — a drop is a drop wherever on this
                    // path it happens.
                    warn!("an encoded video chunk could not be read; asking for a keyframe");
                    control.0.keyframe.set(true);
                    return;
                }
                let keyframe = chunk.type_() == web_sys::EncodedVideoChunkType::Key;
                // Something is dropped when this queue is full, and *which*
                // is the whole question. `try_send` refuses the unit just
                // encoded and keeps the two before it, which is this queue's
                // stated policy exactly backwards: after a scheduling or
                // encoder burst the session is handed a stale picture, then
                // another, before it ever reaches the current scene. The
                // newest frame is the only one worth having — that is what
                // the depth of two is for — so the oldest is evicted instead.
                // The microphone's queue makes the same call one crate over.
                //
                // The keyframe is asked for *here*, and not left to the
                // session: a unit dropped at this queue never reaches the
                // session at all, so its own "my send failed" path cannot see
                // the gap, and every P-frame after this one references a
                // picture the peer will never hold. Asked whenever something
                // was evicted, which `force_send` reports as `Ok(Some(_))`; a
                // closed channel means the call is over and nothing wants a
                // picture.
                if let Ok(Some(_)) = tx.force_send(EncodedFrame { data, keyframe }) {
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
    // Guarded from construction, and declared after the two closures it
    // hands the browser: see `EncoderGuard`.
    let guarded = EncoderGuard(Some(
        web_sys::VideoEncoder::new(&init)
            .map_err(|e| anyhow!("no video encoder: {}", describe(&e)))?,
    ));
    let encoder = guarded.0.as_ref().expect("just armed").clone();
    // The same config the support check asked about, not a second one built
    // to the same recipe: they cannot drift if there is only one.
    encoder.configure(&config).map_err(|e| {
        anyhow!(
            "the encoder refused {CODEC} at these settings: {}",
            describe(&e)
        )
    })?;

    let on_tick = tick(&element, &encoder, &control, quality);
    let timer = window
        .set_interval_with_callback_and_timeout_and_arguments_0(
            on_tick.as_ref().unchecked_ref(),
            // Truncated deliberately, so the timer runs slightly early and
            // the fractional deadline in `tick` decides which firings become
            // frames. Rounding up would make it run *late*, which a deadline
            // cannot correct.
            i32::try_from(1000 / quality.fps.max(1)).unwrap_or(50),
        )
        .map_err(|e| anyhow!("the capture timer would not start: {}", describe(&e)))?;

    // A track ends without anyone asking when the device is unplugged or its
    // permission is revoked mid-call. Nothing else notices: the capture timer
    // goes on handing the encoder whatever the element last showed, so the
    // registry keeps drawing a live camera over a still picture. Closing the
    // frame channel is the same signal the encoder's own error path sends,
    // and `pump_local` already reads it as the device having gone.
    //
    // `stop()` deliberately does *not* fire this — the specification says so —
    // so an ordinary teardown does not come back through here.
    let on_ended: Vec<Closure<dyn FnMut(web_sys::Event)>> = stream
        .get_tracks()
        .iter()
        .filter_map(|track| track.dyn_into::<web_sys::MediaStreamTrack>().ok())
        .map(|track| {
            let tx = tx.clone();
            let ended = Closure::<dyn FnMut(web_sys::Event)>::new(move |_: web_sys::Event| {
                warn!("the camera stopped: its track ended");
                tx.close();
            });
            track.set_onended(Some(ended.as_ref().unchecked_ref()));
            ended
        })
        .collect();

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
            encoder: guarded.release(),
            timer: Some(timer),
            _on_tick: on_tick,
            _on_chunk: on_chunk,
            _on_error: on_error,
            _on_ended: on_ended,
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

    let asked = devices
        .get_user_media_with_constraints(&constraints)
        .map_err(|e| anyhow!("the camera could not be opened: {}", describe(&e)))?;

    // Bounded, because the thing this waits on is a person. `getUserMedia`
    // settles when the permission prompt is answered and not before, and by
    // the time a camera is asked for the *microphone* is already open: a
    // prompt nobody answers would hold this task, the call's audio devices
    // and a hangup that can only be recorded as deferred, for as long as the
    // tab is left alone. Giving up downgrades the call to voice, which is
    // what every other camera failure does.
    let abandoned = Rc::new(Cell::new(false));
    // A prompt answered after we gave up still opens the device, and the
    // stream it resolves with is one nothing here is holding — so its tracks
    // would run, with the tab's indicator on, until the page went away. The
    // same promise is awaited twice, which is what promises are for.
    {
        let abandoned = Rc::clone(&abandoned);
        let late = asked.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let Ok(value) = wasm_bindgen_futures::JsFuture::from(late).await else {
                return;
            };
            if !abandoned.get() {
                return;
            }
            if let Ok(stream) = value.dyn_into::<web_sys::MediaStream>() {
                warn!("the camera opened after the call gave up waiting for it; closing it again");
                stop_tracks(&stream);
            }
        });
    }

    let opened = wasm_bindgen_futures::JsFuture::from(asked);
    let deadline = after(window, PERMISSION_CEILING_MS);
    let Some(opened) = futures_lite::future::or(async move { Some(opened.await) }, async move {
        deadline.await;
        None
    })
    .await
    else {
        abandoned.set(true);
        bail!("the camera permission prompt went unanswered");
    };

    opened
        .map_err(|e| anyhow!("the camera was refused: {}", describe(&e)))?
        .dyn_into::<web_sys::MediaStream>()
        .map_err(|_| anyhow!("the browser opened something that is not a stream"))
}

/// How long a camera permission prompt is waited on.
///
/// Generous, because it is a person reading a dialog, and a call that
/// downgrades to voice while somebody was reaching for the mouse is the worse
/// failure. Short enough that a prompt left on screen does not hold a call's
/// microphone open indefinitely.
const PERMISSION_CEILING_MS: i32 = 30_000;

/// Resolve after `ms`, through the only clock this target has.
///
/// `tokio::time` links here and traps on the first await; the session says
/// the same thing in `exec::sleep`, which this crate has no route to — it
/// depends on nothing above `oxidezap-video`.
async fn after(window: &web_sys::Window, ms: i32) {
    let (tx, rx) = async_channel::bounded::<()>(1);
    let fire = Closure::once_into_js(move || {
        let _ = tx.try_send(());
    });
    if window
        .set_timeout_with_callback_and_timeout_and_arguments_0(fire.unchecked_ref(), ms)
        .is_err()
    {
        // No timer to arm means no ceiling to enforce; waiting forever on a
        // channel nothing will send to is what leaves the other side of the
        // race the only one that can finish, which is the behaviour this had
        // before the ceiling existed.
        warn!("no timer to bound the camera permission prompt with");
    }
    let _ = rx.recv().await;
}

/// A `<video>` playing the stream, so a `VideoFrame` can be taken from it.
///
/// Muted and `playsinline`, and never added to the document: an element with
/// no parent still decodes, and one that were added would draw the self-view
/// twice — once here and once wherever the front end puts the decoded frames.
/// Muted also matters on its own, since autoplay of an unmuted element is
/// refused.
async fn attach(
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

    // Awaited, because a refusal here is a camera that will produce nothing:
    // the tick reads `ready_state` and would sit under it forever, or encode
    // a paused picture, with the offer already out as video. A muted element
    // is normally exempt from autoplay policy, which is why this was written
    // as fire-and-forget — but "normally" is not the same as "always", and
    // `play()` also rejects on a document that is not fully active or a media
    // start that fails outright.
    //
    // Only a *rejection* is an answer, though. `play()` resolves when
    // playback actually begins, and waiting on that unbounded would be the
    // same defect as an unanswered permission prompt — so a slow resolve is
    // not read as anything: the deadline lets setup carry on, and the ticks
    // wait for readiness as they already do.
    let playing = element
        .play()
        .map_err(|e| anyhow!("the camera preview would not start: {}", describe(&e)))?;
    let refused = futures_lite::future::or(
        async {
            wasm_bindgen_futures::JsFuture::from(playing)
                .await
                .err()
                .map(|e| describe(&e))
        },
        async {
            after(window, PLAYBACK_GRACE_MS).await;
            None
        },
    )
    .await;
    if let Some(reason) = refused {
        bail!("the browser would not play the camera's own stream: {reason}");
    }
    Ok(element)
}

/// How long a rejection from `play()` is waited for before setup carries on.
///
/// Not how long playback may take: a resolve means it started and a timeout
/// means nothing at all, so this only bounds how long a *refusal* has to
/// arrive in. Short, because a refusal is decided by policy rather than by
/// the device, and the cost of missing one is the tick logging that the
/// element is not ready.
const PLAYBACK_GRACE_MS: i32 = 2_000;

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
    // `setInterval` takes whole milliseconds, so the timer is armed at the
    // truncated period and runs *fast* — 33 ms is 30.3 fps, 16 ms is 62.5.
    // The stamps above advance by exactly the negotiated stride whatever the
    // timer does, so the two disagree by a fraction of a percent that
    // accumulates: over a long call the video's own clock walks away from the
    // audio's. The truncation cannot be taken out of `setInterval`, so the
    // frames are gated on a fractional deadline instead — the timer may fire
    // early, and a tick that arrives before its frame is due does nothing.
    let period_ms = MILLIS_PER_SECOND / f64::from(quality.fps.max(1));
    let due = Rc::new(Cell::new(f64::NEG_INFINITY));
    let complained = Rc::new(RefCell::new(false));
    Closure::<dyn FnMut()>::new(move || {
        if encoder.state() != web_sys::CodecState::Configured {
            return;
        }
        let now = monotonic_now();
        if now < due.get() {
            return;
        }
        // From the deadline rather than from `now`, so the fractional
        // remainder carries: advancing by the period from whenever the timer
        // happened to fire is the drift this exists to remove. Re-anchored
        // when the timer has been away for more than a frame — a backgrounded
        // tab throttles to seconds, and a schedule catching up on that debt
        // would run flat out for as long as it was gone.
        let next = due.get() + period_ms;
        due.set(if next < now { now + period_ms } else { next });
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
        let wanted_key = control.0.keyframe.replace(false);
        options.set_key_frame(wanted_key);
        if let Err(e) = encoder.encode_with_options(&frame, &options) {
            // The ask outlives the frame it was made of. This encoder is
            // configured with no periodic IDR — every keyframe here is one
            // somebody asked for, whether that was the stream opening, a
            // queue that dropped a unit or the peer's PLI — so consuming the
            // request on a frame that was never submitted leaves a decoder
            // waiting on a keyframe nothing will now produce.
            if wanted_key {
                control.0.keyframe.set(true);
            }
            if !std::mem::replace(&mut complained.borrow_mut(), true) {
                warn!("a camera frame could not be encoded: {}", describe(&e));
            }
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
