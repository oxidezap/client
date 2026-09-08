#![cfg(target_family = "wasm")]

use oxidezap_webcodecs_tests::{decoder, feed};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen(module = "/transform.js")]
extern "C" {
    type TransformDecoder;
    #[wasm_bindgen(constructor)]
    fn new() -> TransformDecoder;
    #[wasm_bindgen(method)]
    fn install(this: &TransformDecoder);
    #[wasm_bindgen(method)]
    fn restore(this: &TransformDecoder);
    #[wasm_bindgen(method)]
    fn finish(this: &TransformDecoder);
    #[wasm_bindgen(method, catch)]
    async fn emit(
        this: &TransformDecoder,
        timestamp: i32,
        bits: u8,
        rotation: u32,
        flip: bool,
        cropped: bool,
    ) -> Result<String, JsValue>;
    #[wasm_bindgen(method)]
    fn matches(
        this: &TransformDecoder,
        bytes: &js_sys::Uint8Array,
        width: u32,
        height: u32,
    ) -> bool;
    #[wasm_bindgen(method, catch)]
    async fn encode(this: &TransformDecoder) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    async fn oversized(this: &TransformDecoder) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = decodeEmit)]
    async fn decode_emit(
        this: &TransformDecoder,
        timestamp: i32,
        bits: u8,
        turn: u32,
        horizontal: bool,
        vertical: bool,
    ) -> Result<String, JsValue>;
}

#[wasm_bindgen_test]
async fn transformed_output_still_obeys_the_visible_pixel_budget() {
    let browser = TransformDecoder::new();
    browser.install();
    let result = decoder(None);
    browser.restore();
    let decoder = result.unwrap();
    decoder.enable_diagnostics(|_, _| {});
    browser.oversized().await.unwrap();
    assert!(decoder.failure().unwrap().contains("1280x721"));
    assert!(decoder.newest().is_none());
    let stats = decoder.diagnostics().unwrap();
    assert_eq!(stats.readbacks_started, 0);
    assert_eq!(stats.failed_frames, 1);
    assert_eq!(stats.latest_display_transform, Some((Some(90), true)));
    assert_eq!(stats.latest_display_dimensions, Some((721, 1280)));
}

#[wasm_bindgen_test]
async fn production_matches_canvas_for_all_frame_and_rtp_transforms() {
    let browser = Rc::new(TransformDecoder::new());
    let notify = Rc::clone(&browser);
    browser.install();
    let result = decoder(Some(Rc::new(move |_| notify.finish())));
    browser.restore();
    let decoder = result.unwrap();
    decoder.enable_diagnostics(|_, _| {});
    let mut failures = Vec::new();
    let mut stamp = 0;
    for rotation in [0, 90, 180, 270] {
        for flip in [false, true] {
            for bits in 0..4 {
                stamp += 1;
                feed(&decoder, stamp, bits);
                let metadata = browser
                    .emit(stamp, bits, rotation, flip, false)
                    .await
                    .unwrap();
                let picture = decoder.newest().unwrap();
                let pixels = picture.image.0[0].buffer();
                let matches = browser.matches(
                    &js_sys::Uint8Array::from(pixels.as_raw().as_slice()),
                    pixels.width(),
                    pixels.height(),
                );
                console_log!("bits={bits} outputmetadata={metadata} canvas_match={matches}");
                if !matches {
                    failures.push((bits, rotation, flip));
                }
            }
        }
    }
    assert_eq!(decoder.diagnostics().unwrap().materialized, 32);
    assert_eq!(decoder.diagnostics().unwrap().display_transform_changes, 8);
    assert_eq!(decoder.diagnostics().unwrap().transformed_outputs, 28);
    assert_eq!(decoder.diagnostics().unwrap().bgra, 4);
    assert!(failures.is_empty(), "Canvas mismatches {failures:?}");
    for bits in [1, 0, 1, 0, 1] {
        stamp += 1;
        feed(&decoder, stamp, bits);
        browser.emit(stamp, bits, 0, false, false).await.unwrap();
        let picture = decoder.newest().unwrap();
        let pixels = picture.image.0[0].buffer();
        assert!(browser.matches(
            &js_sys::Uint8Array::from(pixels.as_raw().as_slice()),
            pixels.width(),
            pixels.height()
        ));
    }
    for rotation in [0, 90, 180, 270] {
        for flip in [false, true] {
            for bits in 0..4 {
                stamp += 1;
                feed(&decoder, stamp, bits);
                browser
                    .emit(stamp, bits, rotation, flip, true)
                    .await
                    .unwrap();
                let picture = decoder.newest().unwrap();
                let pixels = picture.image.0[0].buffer();
                assert_eq!(pixels.as_raw().len(), 3 * 5 * 4);
                assert!(browser.matches(
                    &js_sys::Uint8Array::from(pixels.as_raw().as_slice()),
                    pixels.width(),
                    pixels.height()
                ));
            }
        }
    }
}

#[wasm_bindgen_test]
async fn generated_h264_sei_output_matches_canvas() {
    let browser = Rc::new(TransformDecoder::new());
    browser.encode().await.unwrap();
    let notify = Rc::clone(&browser);
    browser.install();
    let result = decoder(Some(Rc::new(move |_| notify.finish())));
    browser.restore();
    let decoder = result.unwrap();
    let mut stamp = 0;
    for turn in 0..4 {
        for horizontal in [false, true] {
            for vertical in [false, true] {
                for bits in 0..4 {
                    stamp += 1;
                    feed(&decoder, stamp, bits);
                    let metadata = browser
                        .decode_emit(stamp, bits, turn, horizontal, vertical)
                        .await
                        .unwrap();
                    let picture = decoder.newest().unwrap();
                    let pixels = picture.image.0[0].buffer();
                    let matches = browser.matches(
                        &js_sys::Uint8Array::from(pixels.as_raw().as_slice()),
                        pixels.width(),
                        pixels.height(),
                    );
                    console_log!(
                        "sei_ccw={} hor={horizontal} ver={vertical} bits={bits} outputmetadata={metadata} canvas_match={matches}",
                        turn * 90
                    );
                    assert!(matches, "decoded H264 Canvas mismatch");
                }
            }
        }
    }
}
