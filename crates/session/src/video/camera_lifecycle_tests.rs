use std::time::Duration;

use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
export function fakeCamera() {
    const devices = navigator.mediaDevices;
    const getUserMedia = devices.getUserMedia;
    const supports = VideoEncoder.isConfigSupported;
    const play = HTMLMediaElement.prototype.play;
    const canvas = document.createElement('canvas');
    canvas.width = canvas.height = 2;
    canvas.getContext('2d').fillRect(0, 0, 2, 2);
    const stream = canvas.captureStream(1);
    let resolve;
    const permission = new Promise(r => { resolve = r; });
    const state = {
        asked: false,
        preview: null,
        grant() { resolve(stream); },
        ended() { return stream.getTracks().every(t => t.readyState === 'ended'); },
        released() { return !this.preview.isConnected && this.preview.srcObject === null; },
        restore() {
            devices.getUserMedia = getUserMedia;
            VideoEncoder.isConfigSupported = supports;
            HTMLMediaElement.prototype.play = play;
            stream.getTracks().forEach(t => t.stop());
            if (this.preview) {
                this.preview.srcObject = null;
                this.preview.remove();
            }
        }
    };
    devices.getUserMedia = () => { state.asked = true; return permission; };
    VideoEncoder.isConfigSupported = () => Promise.resolve({supported: true});
    HTMLMediaElement.prototype.play = function() {
        state.preview = this;
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
"#)]
extern "C" {
    #[wasm_bindgen(js_name = fakeCamera)]
    fn fake_camera() -> JsValue;
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
        let camera = FakeCamera(fake_camera());
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
    let camera = FakeCamera(fake_camera());
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
