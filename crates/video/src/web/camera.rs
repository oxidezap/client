//! One camera, opened and encoding, for as long as a call holds it.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use anyhow::{Result, anyhow, bail};
use log::{debug, error, warn};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

use crate::EncodedFrame;
use crate::VideoQuality;
use std::time::Duration;

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
            stop_tracks(
                &stream,
                "the camera is closed again: it opened but its setup did not finish",
            );
        }
    }
}

/// End every track, which is what actually releases the device.
///
/// Asked afterwards whether each one really reached `ended`, and says so when
/// one did not. `stop()` sets `readyState` synchronously, so a track still
/// live on the way out of this is a camera this teardown did not release —
/// which is exactly what somebody reports as a tab whose camera light stays
/// on, and precisely what a count of `stop()` *calls* cannot show. The audio
/// graph learned this first; the camera printed "the camera is closed" over
/// the same uncertainty.
fn stop_tracks(stream: &web_sys::MediaStream, what: &str) {
    let mut stopped = 0usize;
    let mut still_live = 0usize;
    for track in stream.get_tracks().iter() {
        if let Ok(track) = track.dyn_into::<web_sys::MediaStreamTrack>() {
            // Disarmed before it is stopped. `stop()` is specified not to fire
            // `ended`, but a handler the browser calls after its closure has
            // been dropped is a trap rather than a missed event, and the
            // closures go with the `Held` that is dropping now.
            track.set_onended(None);
            track.stop();
            if track.ready_state() == web_sys::MediaStreamTrackState::Ended {
                stopped += 1;
            } else {
                still_live += 1;
            }
        }
    }
    if still_live == 0 {
        debug!("{what} ({stopped} track(s) stopped)");
    } else {
        warn!(
            "{what}, but {still_live} track(s) are still live ({stopped} stopped): \
             this tab is still holding the camera"
        );
    }
}

/// Let go of a `<video>` that was wired to a stream.
///
/// Three steps, and the order is the point. A media element playing a
/// `MediaStream` is kept alive by the browser rather than by anything holding
/// it here — playback is a root — so an element merely dropped goes on being a
/// sink on the camera's track for as long as the page lives. Six failed
/// attempts in one call is six of them. Paused, unwired and removed, it is
/// reachable from nothing and holding nothing.
fn release_element(element: &web_sys::HtmlVideoElement) {
    let _ = element.pause();
    element.set_src_object(None);
    element.remove();
}

/// Takes down a preview that was inserted but never handed to [`Held`].
///
/// [`attach`] puts the element in the document and starts it playing, and
/// from there to [`Held`] there are three more fallible steps — the encoder
/// the browser may not build, the configuration it may refuse, the timer that
/// may not arm. Each of them returns through a `?` that used to leave the
/// element where it was: rooted in the document, still wired to a stream, and
/// kept alive by the browser because playback is a root. The `CameraGuard`
/// above stops the tracks on those paths, which is the device; this is the
/// node and the sink on it, and repeated failures accumulate one of each.
struct ElementGuard(Option<web_sys::HtmlVideoElement>);

impl ElementGuard {
    /// Setup succeeded; [`Held`] takes it down from here.
    fn release(mut self) -> web_sys::HtmlVideoElement {
        self.0.take().expect("a guard is released once")
    }
}

impl Drop for ElementGuard {
    fn drop(&mut self) {
        if let Some(element) = self.0.take() {
            release_element(&element);
            debug!("the camera's preview is taken down again: its setup did not finish");
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
    /// What the outbound path did, reported once on the way out. See
    /// [`Capture`].
    capture: Rc<Capture>,
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
        // Paused, unwired and out of the document: it was put there to be
        // allowed to play, so leaving it would accumulate one dead element
        // per call — and a playing one is not garbage the page can collect.
        release_element(&self.element);
        stop_tracks(&self.stream, "the camera is closed");
        // After the device is released, because this is the sentence somebody
        // reads when the picture never arrived and it should be the last word
        // on the subject.
        self.capture.report();
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
    // Guarded from the moment it is in the document, for the same reason the
    // camera is guarded from the moment it is open: see `ElementGuard`.
    let preview = ElementGuard(Some(attach(&window, &stream).await?));
    let element = preview.0.as_ref().expect("just armed").clone();

    let (tx, rx) = async_channel::bounded::<EncodedFrame>(FRAME_DEPTH);
    // Before the callback rather than after, so the chunk handler can ask for
    // a keyframe when it is the one that drops a unit.
    let control = CameraControl(Rc::new(Control {
        keyframe: Cell::new(true),
    }));
    let capture = Rc::new(Capture::default());
    let on_chunk = {
        let tx = tx.clone();
        let control = control.clone();
        let capture = Rc::clone(&capture);
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
                    capture.dropped.set(capture.dropped.get().saturating_add(1));
                    return;
                }
                let keyframe = chunk.type_() == web_sys::EncodedVideoChunkType::Key;
                let n = capture.chunks.get().saturating_add(1);
                capture.chunks.set(n);
                if keyframe {
                    capture
                        .keyframes
                        .set(capture.keyframes.get().saturating_add(1));
                }
                if n == 1 {
                    // The one line that separates "the encoder never answered"
                    // from every fault downstream of it. An encoder that
                    // configures, accepts frames and emits nothing is a stream
                    // that never existed, and it looks exactly like a stream
                    // the session threw away.
                    debug!(
                        "the encoder produced its first chunk ({} bytes, {})",
                        data.len(),
                        if keyframe {
                            "keyframe"
                        } else {
                            "not a keyframe"
                        }
                    );
                    // And whether it is the shape everything downstream
                    // assumes. The library splits access units on Annex-B
                    // start codes and yields *nothing* for a buffer without
                    // one — silently, with no packet and no error — so an
                    // encoder that ignored `avc.format` would look, from
                    // every log we have, exactly like a working one whose
                    // peer cannot see it. AVCC's leading length word is four
                    // bytes that read almost identically at a glance, which
                    // is why counting bytes never caught it. One check, once.
                    if !data.starts_with(&[0, 0, 0, 1]) && !data.starts_with(&[0, 0, 1]) {
                        error!(
                            "the encoder is not emitting Annex-B: the first chunk begins \
                             {:02x?} — the media plane will discard every frame of this call",
                            &data[..data.len().min(8)]
                        );
                    }
                }
                // Something is dropped when this queue is full, and *which*
                // is the whole question — with the opposite answer to the
                // microphone's, which evicts its oldest frame. The difference
                // is that PCM frames are independent and H.264 pictures are
                // not: a queued P-frame references the one in front of it, so
                // evicting the oldest here does not free a slot, it makes
                // everything still queued undecodable and then sends it. The
                // peer would receive two corrupt pictures where refusing the
                // new one sends two good ones and a gap.
                //
                // So the newest is refused. It is the staler choice by two
                // frames — 66 ms at 30 fps — and that is the whole cost of
                // keeping what is delivered decodable.
                //
                // The keyframe is asked for *here* either way, and not left
                // to the session: a unit dropped at this queue never reaches
                // the session at all, so its own "my send failed" path cannot
                // see the gap, and every P-frame after this one references a
                // picture the peer will never hold. Only on `Full` — a closed
                // channel means the call is over and nothing wants a picture.
                if let Err(async_channel::TrySendError::Full(_)) =
                    tx.try_send(EncodedFrame { data, keyframe })
                {
                    control.request_keyframe();
                    let dropped = capture.dropped.get().saturating_add(1);
                    capture.dropped.set(dropped);
                    if dropped == 1 {
                        debug!("an encoded video chunk was dropped: the session is not keeping up");
                    }
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

    let on_tick = tick(&element, &encoder, &control, quality, &capture);
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
            capture,
            // Setup finished: the guard hands the camera to `Held`, which is
            // what closes it from here.
            stream: guard.release(),
            element: preview.release(),
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
                stop_tracks(&stream, "the late camera is closed");
            }
        });
    }

    let opened = wasm_bindgen_futures::JsFuture::from(asked);
    let deadline = oxidezap_platform::sleep(Duration::from_millis(PERMISSION_CEILING_MS as u64));
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

/// A `<video>` playing the stream, so a `VideoFrame` can be taken from it.
///
/// Muted and `playsinline`, and *in* the document — one pixel of it, moved off
/// screen and fully transparent. It was written detached, on the reasoning
/// that an element with no parent still decodes and an added one would draw
/// the self-view twice; the second half is answered by the styling instead,
/// and the first is what production disagreed with. Every camera a call tried
/// to open failed with
///
/// > The play() request was interrupted because the media was removed from
/// > the document.
///
/// which is Blink rejecting a pending play promise for a lifecycle reason,
/// and the one thing it names is the document. `display: none` is not the way
/// to hide it — a hidden element is entitled to stop rendering, and this one
/// exists precisely to produce frames.
///
/// Taken down again by [`Held`] at the end of the call, by [`ElementGuard`]
/// if setup fails after this returns, and by this function on the way out of
/// its own failure: six failed attempts in one call is six elements, and a
/// leak of them is a leak of the streams they hold.
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
    let _ = element.set_attribute(
        "style",
        "position:fixed;top:-9999px;left:-9999px;width:1px;height:1px;\
         opacity:0;pointer-events:none",
    );
    // Refused rather than skipped when there is nowhere to put it. Carrying
    // on with a detached element is carrying on with the exact configuration
    // this function exists to stop using, and its failure is six lines
    // further down and reads like an autoplay refusal.
    document
        .body()
        .ok_or_else(|| anyhow!("no document body to play the camera in"))?
        .append_child(&element)
        .map_err(|e| {
            anyhow!(
                "the camera's preview would not go in the document: {}",
                describe(&e)
            )
        })?;
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
    let started = match element.play() {
        Ok(started) => started,
        Err(e) => {
            // The element is in the document by now, so this exit has to take
            // it down itself: `ElementGuard` only covers the failures *after*
            // this function returns one.
            release_element(&element);
            bail!("the camera preview would not start: {}", describe(&e));
        }
    };
    let refused = futures_lite::future::or(
        async {
            wasm_bindgen_futures::JsFuture::from(started)
                .await
                .err()
                .map(|e| describe(&e))
        },
        async {
            oxidezap_platform::sleep(Duration::from_millis(PLAYBACK_GRACE_MS as u64)).await;
            None
        },
    )
    .await;
    // The promise is not the question; whether frames will come is. The two
    // came apart in production: `play()` was aborted for a lifecycle reason
    // while the element went on decoding perfectly well, and treating the
    // promise as the answer downgraded every video call to voice. So the
    // element is asked, and it is asked whatever the promise did — a
    // rejection, a resolution, and a promise still pending at the grace all
    // reach the same test. Asking only on a rejection would let the one case
    // that never answers through untested.
    //
    // `paused` is the load-bearing half. Readiness alone is not playback
    // here: a `MediaStream` reaches `HAVE_CURRENT_DATA` with a nonzero
    // `videoWidth` as soon as the element is wired to it, whether or not it
    // was ever allowed to start — so an element genuinely refused by autoplay
    // policy passes the readiness test and then hands the encoder one still
    // picture for the length of the call. The two together are the question
    // the capture tick asks plus the one it cannot: playing, and showing
    // something.
    if element.paused() || element.ready_state() < 2 || element.video_width() == 0 {
        release_element(&element);
        match refused {
            Some(reason) => bail!("the browser would not play the camera's own stream: {reason}"),
            None => bail!(
                "the camera's preview never started playing, and said nothing about why in \
                 {PLAYBACK_GRACE_MS}ms"
            ),
        }
    }
    // Only reachable with the element playing, which is why this is a warning
    // and not the failure above.
    if let Some(reason) = refused {
        warn!("the camera's preview reported {reason}, but it is playing; carrying on");
    }
    Ok(element)
}

/// How long `play()` is given to say anything before the element is asked.
///
/// Not how long playback may take, and not a verdict of its own: whatever
/// this expires on, the element is still asked whether it is playing, so a
/// timeout costs a decision rather than making one. Short, because a refusal
/// is decided by policy rather than by the device — the device having already
/// answered, upstream, when `getUserMedia` returned.
const PLAYBACK_GRACE_MS: i32 = 2_000;

/// One capture tick: take what the element is showing and encode it.
/// What one camera's outbound path did, so a log can say where it stopped.
///
/// The relay learned this in #62 and #70 and it is the same lesson: a stage
/// that fails by doing nothing is invisible, and four production logs went by
/// before one said which stage. Outbound video has five places it can stop —
/// the tick can decline to submit for three separate reasons, the encoder can
/// emit nothing, and the queue to the session can refuse everything — and
/// until now every one of them looked identical from a log: silence, with the
/// camera reporting itself open the whole time.
///
/// Firsts and totals rather than a line per frame, for the same reason the
/// relay reports firsts: twenty frames a second is not a log, and what has to
/// be answerable is "did this stage ever happen", not "how is it doing now".
#[derive(Default)]
struct Capture {
    submitted: Cell<u64>,
    chunks: Cell<u64>,
    /// IDRs among those chunks. Counted because one keyframe followed by two
    /// hundred P-frames and a healthy stream are the same two numbers above,
    /// and the difference between them is the whole fault this backend had:
    /// the library drops every non-IDR while a keyframe gate is closed.
    keyframes: Cell<u64>,
    /// Encoded units that never reached the session, at either hop.
    dropped: Cell<u64>,
    /// One per reason, so a reason is explained the first time it bites and
    /// then only counted. A tick that skips twenty times a second would bury
    /// the log it is supposed to be writing.
    said: RefCell<HashSet<&'static str>>,
}

impl Capture {
    /// Say `why` if this is the first time, and count it either way.
    fn skipping(&self, why: &'static str) {
        if self.said.borrow_mut().insert(why) {
            debug!("the camera is not submitting frames: {why}");
        }
    }

    /// One line naming every stage, said when the camera closes.
    ///
    /// A camera that submitted nothing, one whose encoder answered nothing,
    /// and one whose frames were all refused by the session read identically
    /// without this — and are three unrelated faults.
    fn report(&self) {
        debug!(
            "the camera encoded {} chunk(s) ({} keyframe(s)) from {} submitted frame(s), \
             {} dropped on the way out",
            self.chunks.get(),
            self.keyframes.get(),
            self.submitted.get(),
            self.dropped.get()
        );
    }
}

fn tick(
    element: &web_sys::HtmlVideoElement,
    encoder: &web_sys::VideoEncoder,
    control: &CameraControl,
    quality: VideoQuality,
    capture: &Rc<Capture>,
) -> Closure<dyn FnMut()> {
    let element = element.clone();
    let encoder = encoder.clone();
    let control = control.clone();
    let capture = Rc::clone(capture);
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
    // Frames since the last one that asked for an IDR; see the cadence below.
    let keyed = Rc::new(Cell::new(0u64));
    let complained = Rc::new(RefCell::new(false));
    Closure::<dyn FnMut()>::new(move || {
        if encoder.state() != web_sys::CodecState::Configured {
            capture.skipping("the encoder is no longer configured");
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
            // Ordinary once in a while and a stopped stream if it never
            // clears: an encoder that accepts frames and emits nothing wedges
            // here permanently, which is silence indistinguishable from a
            // camera that was never asked for anything.
            capture.skipping("the encoder's queue is not draining");
            return;
        }
        if element.ready_state() < 2 || element.video_width() == 0 {
            // Nothing is playing yet: the element is still opening the
            // stream. Not an error at the start of a call, and not worth a
            // frame of duplicated black — but the same branch is also what a
            // preview that stopped playing mid-call falls into forever.
            capture.skipping("the preview has no frame to take yet");
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
        // Asked for on a cadence as well as on request, which is not belt and
        // braces: it is the contract the desktop backend has always met and
        // this one did not. The library drops every access unit that is not
        // an IDR while one of its keyframe gates is closed, raises those
        // gates on backpressure and on a relay reconnect, and asks for a
        // keyframe by publishing an event — so a backend that only ever emits
        // a requested IDR sends nothing at all for the rest of the call the
        // first time such a request is missed. See `KEYFRAME_SECONDS`.
        let since_key = keyed.get().saturating_add(1);
        let fps = u64::from(quality.fps.max(1));
        let due_a_key = since_key >= fps * u64::from(crate::KEYFRAME_SECONDS);
        // A request is honoured, but not sooner than the last IDR plus
        // `MIN_REQUESTED_KEYFRAME_SECONDS` — and *not consumed* when it is
        // too soon, so the ask survives to the first tick that may serve it.
        // A burst of requests describing one loss therefore costs one
        // keyframe, and the last request in a burst is never the one lost.
        // Counted in frames because the tick already counts them; a page has
        // no clock this loop can read for free.
        let asked = control.0.keyframe.get();
        let may_answer = since_key >= fps * u64::from(crate::MIN_REQUESTED_KEYFRAME_SECONDS);
        if asked && may_answer {
            control.0.keyframe.set(false);
        }
        let wanted_key = (asked && may_answer) || due_a_key;
        keyed.set(if wanted_key { 0 } else { since_key });
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
        let n = capture.submitted.get().saturating_add(1);
        capture.submitted.set(n);
        if n == 1 {
            debug!("the camera submitted its first frame to the encoder");
        }
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
