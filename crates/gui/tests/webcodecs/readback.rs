#![cfg(target_family = "wasm")]

use std::cell::RefCell;
use std::rc::Rc;

use oxidezap_webcodecs_tests::{Decoder, Picture, decoder, feed, images_created};
use wasm_bindgen::prelude::*;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen(module = "/fixture.js")]
extern "C" {
    type ControlledDecoder;
    #[wasm_bindgen(constructor)]
    fn new() -> ControlledDecoder;
    #[wasm_bindgen(method)]
    fn install(this: &ControlledDecoder, fail_configure: bool);
    #[wasm_bindgen(method)]
    fn restore(this: &ControlledDecoder);
    #[wasm_bindgen(method)]
    fn emit(this: &ControlledDecoder, timestamp: i32, width: u32, height: u32);
    #[wasm_bindgen(method, catch)]
    async fn settle(this: &ControlledDecoder, index: u32, reject: bool) -> Result<(), JsValue>;
    #[wasm_bindgen(method)]
    fn count(this: &ControlledDecoder) -> u32;
    #[wasm_bindgen(method)]
    fn stamp(this: &ControlledDecoder, index: u32) -> i32;
    #[wasm_bindgen(method)]
    fn closed(this: &ControlledDecoder, index: u32) -> bool;
    #[wasm_bindgen(method)]
    fn reused(this: &ControlledDecoder, a: u32, b: u32) -> bool;
    #[wasm_bindgen(method, js_name = lengthReads)]
    fn length_reads(this: &ControlledDecoder, index: u32) -> u32;
    #[wasm_bindgen(method, js_name = closeCount)]
    fn close_count(this: &ControlledDecoder) -> u32;
    #[wasm_bindgen(method, js_name = decodeCount)]
    fn decode_count(this: &ControlledDecoder) -> u32;
    #[wasm_bindgen(method)]
    fn cleanup(this: &ControlledDecoder);
    #[wasm_bindgen(catch)]
    async fn drain() -> Result<(), JsValue>;
}

struct Scenario {
    decoder: Option<Decoder>,
    browser: ControlledDecoder,
    published: Rc<RefCell<Vec<Picture>>>,
    initial_images: usize,
}

impl Scenario {
    fn new(live: bool) -> Self {
        let browser = ControlledDecoder::new();
        let published = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&published);
        let sink: Option<Rc<dyn Fn(Picture)>> =
            live.then(|| Rc::new(move |p| sink.borrow_mut().push(p)) as Rc<dyn Fn(Picture)>);
        browser.install(false);
        let result = decoder(sink);
        browser.restore();
        Self {
            decoder: Some(result.expect("configure test decoder")),
            browser,
            published,
            initial_images: images_created(),
        }
    }

    fn decoder(&self) -> &Decoder {
        self.decoder.as_ref().unwrap()
    }

    fn emit(&self, stamp: i32) {
        feed(self.decoder(), stamp, 0);
        self.browser.emit(stamp, 3, 2);
    }

    fn stamps(&self) -> Vec<i64> {
        self.published
            .borrow()
            .iter()
            .map(|p| p.timestamp_micros)
            .collect()
    }

    fn images(&self) -> usize {
        images_created() - self.initial_images
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        self.decoder.take();
        self.browser.cleanup();
    }
}

#[wasm_bindgen_test]
async fn active_a_pending_b_replaced_by_c_then_c_publishes() {
    let s = Scenario::new(true);
    s.emit(1);
    drain().await.unwrap();
    s.emit(2);
    s.emit(3);
    drain().await.unwrap();
    assert_eq!(s.browser.count(), 1);
    assert!(!s.browser.closed(0));
    assert!(s.browser.closed(1), "B closes without copying");
    assert!(!s.browser.closed(2));
    assert_eq!(
        s.browser.decode_count(),
        3,
        "no compressed input was dropped"
    );
    assert!(s.stamps().is_empty());

    s.browser.settle(0, false).await.unwrap();
    assert!(s.browser.closed(0));
    assert_eq!(s.stamps(), [1]);
    assert_eq!(s.browser.count(), 2);
    assert_eq!(s.browser.stamp(1), 3);
    assert!(s.browser.reused(0, 1), "A's destination is now C's");
    s.browser.settle(1, false).await.unwrap();
    assert!(s.browser.closed(2));
    assert_eq!(s.stamps(), [1, 3]);
    assert_eq!(s.images(), 2);
    assert_eq!(s.decoder().newest().unwrap().timestamp_micros, 3);
    assert!(!s.decoder().take_refusal());

    let pictures = s.published.borrow();
    let pixels = pictures[1].image.0[0].buffer();
    assert_eq!(pixels.dimensions(), (3, 2));
    assert_eq!(&pixels.as_raw()[..8], &[80, 40, 0, 255, 80, 40, 1, 255]);
}

#[wasm_bindgen_test]
async fn reset_closes_pending_and_keeps_one_active_copy_across_generations() {
    let s = Scenario::new(true);
    s.emit(1);
    drain().await.unwrap();
    s.emit(2);
    s.decoder().reset();
    assert!(s.browser.closed(1));
    s.emit(3);
    drain().await.unwrap();
    assert_eq!(
        s.browser.count(),
        1,
        "old copy still owns the readback slot"
    );
    s.browser.settle(0, false).await.unwrap();
    assert_eq!(s.images(), 0);
    assert!(s.stamps().is_empty());
    assert_eq!(s.browser.count(), 2);
    assert_eq!(s.browser.stamp(1), 3);
    s.browser.settle(1, false).await.unwrap();
    assert_eq!(s.stamps(), [3]);
    assert_eq!(s.images(), 1);
}

#[wasm_bindgen_test]
async fn drop_closes_pending_and_rejects_active_completion_before_materialization() {
    let mut s = Scenario::new(true);
    s.emit(1);
    drain().await.unwrap();
    s.emit(2);
    s.decoder.take();
    assert_eq!(s.browser.close_count(), 1);
    assert!(s.browser.closed(1));
    s.browser.settle(0, false).await.unwrap();
    assert!(s.browser.closed(0));
    assert_eq!(s.browser.count(), 1);
    assert_eq!(s.browser.length_reads(0), 0, "no to_vec after drop");
    assert_eq!(s.images(), 0);
    assert!(s.stamps().is_empty());
}

#[wasm_bindgen_test]
async fn rejected_copy_closes_pending_and_reset_can_resume() {
    let s = Scenario::new(true);
    s.emit(1);
    drain().await.unwrap();
    s.emit(2);
    s.browser.settle(0, true).await.unwrap();
    assert!(s.browser.closed(0));
    assert!(s.browser.closed(1));
    assert_eq!(s.browser.count(), 1);
    assert!(
        s.decoder()
            .failure()
            .unwrap()
            .contains("could not read a decoded frame")
    );
    assert_eq!(s.images(), 0);
    s.decoder().reset();
    s.emit(3);
    drain().await.unwrap();
    assert_eq!(s.browser.count(), 2, "rejection released the active slot");
    s.browser.settle(1, false).await.unwrap();
    assert_eq!(s.stamps(), [3]);
    assert!(s.decoder().failure().is_none());
}

#[wasm_bindgen_test]
async fn old_generation_rejection_does_not_fail_the_reset_decoder() {
    let s = Scenario::new(true);
    s.emit(1);
    drain().await.unwrap();
    s.decoder().reset();
    s.emit(2);
    s.browser.settle(0, true).await.unwrap();
    assert!(s.decoder().failure().is_none());
    assert_eq!(s.browser.count(), 2);
    assert_eq!(s.images(), 0);
    s.browser.settle(1, false).await.unwrap();
    assert_eq!(s.stamps(), [2]);
}

#[wasm_bindgen_test]
async fn attachment_stale_completion_skips_to_vec_and_image_creation() {
    let s = Scenario::new(false);
    s.emit(1);
    s.emit(2);
    drain().await.unwrap();
    assert_eq!(s.browser.count(), 2, "attachments still copy concurrently");
    s.browser.settle(1, false).await.unwrap();
    assert_eq!(s.images(), 1);
    s.browser.settle(0, false).await.unwrap();
    assert_eq!(s.browser.length_reads(0), 0, "no to_vec for an older copy");
    assert_eq!(s.images(), 1);
    assert_eq!(s.decoder().newest().unwrap().timestamp_micros, 2);
}

#[wasm_bindgen_test]
async fn idle_buffer_is_reused_but_resize_replaces_it() {
    let s = Scenario::new(true);
    s.emit(1);
    drain().await.unwrap();
    s.browser.settle(0, false).await.unwrap();
    s.emit(2);
    drain().await.unwrap();
    assert!(s.browser.reused(0, 1));
    s.browser.settle(1, false).await.unwrap();
    s.browser.emit(3, 4, 2);
    drain().await.unwrap();
    assert!(!s.browser.reused(1, 2));
    s.browser.settle(2, false).await.unwrap();
    assert_eq!(s.stamps(), [1, 2, 3]);
    assert_eq!(
        s.published.borrow()[2].image.0[0].buffer().dimensions(),
        (4, 2)
    );
}

#[wasm_bindgen_test]
async fn pending_frame_keeps_its_recorded_rotation() {
    let s = Scenario::new(true);
    s.emit(1);
    drain().await.unwrap();
    feed(s.decoder(), 2, 1);
    s.browser.emit(2, 3, 2);
    feed(s.decoder(), 3, 0);
    s.browser.settle(0, false).await.unwrap();
    s.browser.settle(1, false).await.unwrap();
    let pictures = s.published.borrow();
    let pixels = pictures[1].image.0[0].buffer();
    assert_eq!(pixels.dimensions(), (2, 3));
    let red: Vec<_> = pixels
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| p[2])
        .collect();
    assert_eq!(red, [2, 5, 1, 4, 0, 3], "device Cw90 is undone as Cw270");
}

#[wasm_bindgen_test]
fn configure_failure_closes_the_decoder() {
    let browser = ControlledDecoder::new();
    browser.install(true);
    let result = decoder(None);
    browser.restore();
    assert!(result.is_err());
    assert_eq!(browser.close_count(), 1);
    browser.cleanup();
}
