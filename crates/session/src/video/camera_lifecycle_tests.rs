use std::time::Duration;

use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
export function fakeCamera(opened) {
    const devices = navigator.mediaDevices;
    const getUserMedia = devices.getUserMedia;
    const supports = VideoEncoder.isConfigSupported;
    const play = HTMLMediaElement.prototype.play;
    const cloneStream = MediaStream.prototype.clone;
    const cloneTrack = MediaStreamTrack.prototype.clone;
    const setInterval = window.setInterval;
    const clearInterval = window.clearInterval;
    const encode = VideoEncoder.prototype.encode;
    const canvas = document.createElement('canvas');
    canvas.width = canvas.height = 2;
    canvas.getContext('2d').fillRect(0, 0, 2, 2);
    const stream = canvas.captureStream(1);
    const tracks = new Set(stream.getTracks());
    const timers = new Set();
    const errors = [];
    const onError = event => errors.push(event.message);
    window.addEventListener('error', onError);
    let resolve;
    const permission = new Promise(r => { resolve = r; });
    const state = {
        asked: false,
        opens: 0,
        clones: 0,
        ticks: 0,
        encoded: 0,
        preview: null,
        grant() { resolve(stream); },
        ended() { return [...tracks].every(t => t.readyState === 'ended'); },
        released() { return !this.preview.isConnected && this.preview.srcObject === null; },
        status() {
            return JSON.stringify({opens: this.opens, clones: this.clones,
                live: [...tracks].filter(t => t.readyState !== 'ended').length,
                handlers: [...tracks].filter(t => t.onended !== null).length,
                timers: timers.size, ticks: this.ticks, encoded: this.encoded, errors});
        },
        dispatchEnded() { tracks.forEach(t => t.dispatchEvent(new Event('ended'))); },
        clean() {
            return this.opens === 1 && this.clones === 0 &&
                this.ended() && this.released() && timers.size === 0 &&
                [...tracks].every(t => t.onended === null) && errors.length === 0;
        },
        restore() {
            devices.getUserMedia = getUserMedia;
            VideoEncoder.isConfigSupported = supports;
            HTMLMediaElement.prototype.play = play;
            MediaStream.prototype.clone = cloneStream;
            MediaStreamTrack.prototype.clone = cloneTrack;
            window.setInterval = setInterval;
            window.clearInterval = clearInterval;
            VideoEncoder.prototype.encode = encode;
            window.removeEventListener('error', onError);
            timers.forEach(t => clearInterval(t));
            tracks.forEach(t => { t.onended = null; t.stop(); });
            if (this.preview) {
                this.preview.srcObject = null;
                this.preview.remove();
            }
        }
    };
    MediaStream.prototype.clone = function() {
        const cloned = cloneStream.call(this);
        state.clones++;
        cloned.getTracks().forEach(t => tracks.add(t));
        return cloned;
    };
    MediaStreamTrack.prototype.clone = function() {
        const cloned = cloneTrack.call(this);
        state.clones++;
        tracks.add(cloned);
        return cloned;
    };
    window.setInterval = function(callback, delay, ...args) {
        const timer = setInterval(() => { state.ticks++; callback(...args); }, delay);
        timers.add(timer);
        return timer;
    };
    window.clearInterval = function(timer) {
        timers.delete(timer);
        clearInterval(timer);
    };
    VideoEncoder.prototype.encode = function(...args) {
        state.encoded++;
        return encode.apply(this, args);
    };
    devices.getUserMedia = constraints => {
        if (!constraints.video || constraints.audio) throw new Error('expected video-only acquisition');
        state.asked = true;
        state.opens++;
        return opened ? Promise.resolve(stream) : permission;
    };
    if (!opened) VideoEncoder.isConfigSupported = () => Promise.resolve({supported: true});
    HTMLMediaElement.prototype.play = function() {
        state.preview = this;
        if (opened) return play.call(this);
        return new Promise(() => {});
    };
    return state;
}
export function cameraAsked(state) { return state.asked; }
export function cameraPreview(state) { return state.preview !== null; }
export function grantCamera(state) { state.grant(); }
export function cameraEnded(state) { return state.ended(); }
export function previewReleased(state) { return state.released(); }
export function restoreCamera(state) { state.restore(); }
export function cameraStatus(state) { return state.status(); }
export function cameraClean(state) { return state.clean(); }
export function dispatchCameraEnded(state) { state.dispatchEnded(); }
"#)]
extern "C" {
    #[wasm_bindgen(js_name = fakeCamera)]
    fn fake_camera(opened: bool) -> JsValue;
    #[wasm_bindgen(js_name = cameraStatus)]
    fn camera_status(state: &JsValue) -> String;
    #[wasm_bindgen(js_name = cameraClean)]
    fn camera_clean(state: &JsValue) -> bool;
    #[wasm_bindgen(js_name = dispatchCameraEnded)]
    fn dispatch_camera_ended(state: &JsValue);
    #[wasm_bindgen(js_name = cameraAsked)]
    fn camera_asked(state: &JsValue) -> bool;
    #[wasm_bindgen(js_name = cameraPreview)]
    fn camera_preview(state: &JsValue) -> bool;
    #[wasm_bindgen(js_name = grantCamera)]
    fn grant_camera(state: &JsValue);
    #[wasm_bindgen(js_name = cameraEnded)]
    fn camera_ended(state: &JsValue) -> bool;
    #[wasm_bindgen(js_name = previewReleased)]
    fn preview_released(state: &JsValue) -> bool;
    #[wasm_bindgen(js_name = restoreCamera)]
    fn restore_camera(state: &JsValue);
}

struct FakeCamera(JsValue);

impl Drop for FakeCamera {
    fn drop(&mut self) {
        restore_camera(&self.0);
    }
}

async fn yield_to_browser() {
    oxidezap_platform::sleep(Duration::from_millis(1)).await;
}

#[wasm_bindgen_test::wasm_bindgen_test]
async fn cancelled_permission_open_stops_a_late_or_queued_stream() {
    for queued in [false, true] {
        let camera = FakeCamera(fake_camera(false));
        let mut opening = Box::pin(oxidezap_video::open_camera(
            oxidezap_video::VideoQuality::from_environment(),
        ));
        for _ in 0..100 {
            assert!(
                futures_lite::future::poll_once(&mut opening)
                    .await
                    .is_none()
            );
            if camera_asked(&camera.0) {
                break;
            }
            yield_to_browser().await;
        }
        assert!(camera_asked(&camera.0));
        if queued {
            grant_camera(&camera.0);
            yield_to_browser().await;
        }
        drop(opening);
        if !queued {
            grant_camera(&camera.0);
        }
        yield_to_browser().await;
        assert!(camera_ended(&camera.0), "cancelled open left a live track");
    }
}

#[wasm_bindgen_test::wasm_bindgen_test]
async fn cancelled_preview_open_releases_the_element_and_tracks() {
    let camera = FakeCamera(fake_camera(false));
    let mut opening = Box::pin(oxidezap_video::open_camera(
        oxidezap_video::VideoQuality::from_environment(),
    ));
    grant_camera(&camera.0);
    for _ in 0..100 {
        assert!(
            futures_lite::future::poll_once(&mut opening)
                .await
                .is_none()
        );
        if camera_preview(&camera.0) {
            break;
        }
        yield_to_browser().await;
    }
    assert!(camera_preview(&camera.0));
    drop(opening);
    assert!(camera_ended(&camera.0));
    assert!(
        preview_released(&camera.0),
        "cancelled playback left its preview attached"
    );
}

#[wasm_bindgen_test::wasm_bindgen_test]
async fn opened_camera_owner_releases_every_track_before_joining_pumps() {
    use super::*;

    for explicit_stop in [true, false] {
        let camera = FakeCamera(fake_camera(true));
        let (sender, mut published) = tokio::sync::mpsc::channel(2);
        let lost = Arc::new(AtomicBool::new(false));
        let reported = lost.clone();
        let (owner, endpoints) = open(
            slot("camera-lifecycle"),
            VideoPublisher {
                sender: Arc::new(std::sync::Mutex::new(Some(sender))),
                watched: Arc::new(AtomicBool::new(true)),
            },
            Arc::new(move |_, _| reported.store(true, Ordering::Relaxed)),
            Arc::new(|_| {}),
        )
        .await
        .expect("production camera and H.264 encoder must open");
        owner.live();
        let source = endpoints.source.frames();
        let sink = endpoints.sink.clone();
        let control = owner.camera.as_ref().unwrap().control();
        let alive = owner.alive.clone();
        let frame = futures_lite::future::or(async { published.recv().await }, async {
            oxidezap_platform::sleep(Duration::from_secs(5)).await;
            None
        })
        .await
        .expect("real capture loop must produce an encoded frame through LocalVideo");
        drop(frame);
        assert!(owner.alive());

        if explicit_stop {
            let mut stopping = Box::pin(owner.stop());
            assert!(
                futures_lite::future::poll_once(&mut stopping)
                    .await
                    .is_none()
            );
            dispatch_camera_ended(&camera.0);
            assert!(
                camera_clean(&camera.0),
                "before joins: {}",
                camera_status(&camera.0)
            );
            stopping.await;
        } else {
            drop(owner);
            dispatch_camera_ended(&camera.0);
            assert!(
                camera_clean(&camera.0),
                "owner drop: {}",
                camera_status(&camera.0)
            );
        }
        let stopped = camera_status(&camera.0);
        control.request_keyframe();
        dispatch_camera_ended(&camera.0);
        oxidezap_platform::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            camera_status(&camera.0),
            stopped,
            "teardown must not leave callbacks or reacquire"
        );
        assert!(!alive.load(Ordering::Relaxed));
        assert!(!lost.load(Ordering::Relaxed));
        assert!(sink.is_closed());
        assert!(source.is_closed());
        assert!(camera_clean(&camera.0));
    }
}
