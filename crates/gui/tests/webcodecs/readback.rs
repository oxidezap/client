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
    #[wasm_bindgen(method)]
    fn finish(this: &ControlledDecoder);
    #[wasm_bindgen(method, js_name = rgbaOnly)]
    fn rgba_only(this: &ControlledDecoder);
    #[wasm_bindgen(method, js_name = ignoreFormat)]
    fn ignore_format(this: &ControlledDecoder);
    #[wasm_bindgen(method, js_name = rejectAllocation)]
    fn reject_allocation(this: &ControlledDecoder);
    #[wasm_bindgen(method)]
    fn format(this: &ControlledDecoder, index: u32) -> String;
    #[wasm_bindgen(method, catch)]
    async fn benchmark(
        this: &ControlledDecoder,
        width: u32,
        height: u32,
        format: &str,
        rounds: u32,
        peer: &ControlledDecoder,
    ) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(catch)]
    async fn drain() -> Result<(), JsValue>;
}

#[cfg(feature = "benchmarks")]
#[wasm_bindgen_test]
async fn production_readback_benchmark() {
    for format in [
        "decoded:no-preference",
        "decoded:prefer-hardware",
        "decoded:prefer-software",
        "RGBA",
        "I420",
    ] {
        for (width, height) in [(1280, 720)] {
            let browser = Rc::new(ControlledDecoder::new());
            browser.rgba_only();
            let notify = Rc::clone(&browser);
            browser.install(false);
            let baseline = decoder(Some(Rc::new(move |_| notify.finish()))).unwrap();
            browser.restore();
            let peer = Rc::new(ControlledDecoder::new());
            let notify = Rc::clone(&peer);
            peer.install(false);
            let candidate = decoder(Some(Rc::new(move |_| notify.finish()))).unwrap();
            peer.restore();
            console_log!(
                "{}",
                browser
                    .benchmark(width, height, format, 120, &peer)
                    .await
                    .unwrap()
                    .as_string()
                    .unwrap()
            );
            drop((baseline, candidate));
            browser.cleanup();
            peer.cleanup();
        }
    }
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
    s.decoder().enable_diagnostics(|_, _| {});
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
    let stats = s.decoder().diagnostics().unwrap();
    assert_eq!(stats.decoded_outputs, 3);
    assert_eq!(stats.readbacks_started, 2);
    assert_eq!(stats.readbacks_completed, 2);
    assert_eq!(stats.materialized, 2);
    assert_eq!(stats.dropped_pending, 1);
    assert_eq!(stats.dropped_obsolete, 0);
    assert_eq!(stats.bytes_requested, 48);
    assert_eq!(stats.bytes_materialized, 48);
    assert_eq!(stats.bgra, 2);
    assert_eq!(stats.rgba, 0);
    assert_eq!(stats.latest_dimensions, Some((3, 2)));

    let pictures = s.published.borrow();
    let pixels = pictures[1].image.0[0].buffer();
    assert_eq!(pixels.dimensions(), (3, 2));
    assert_eq!(&pixels.as_raw()[..8], &[80, 40, 0, 255, 80, 40, 1, 255]);
}

#[wasm_bindgen_test]
async fn reset_closes_pending_and_keeps_one_active_copy_across_generations() {
    let s = Scenario::new(true);
    s.decoder().enable_diagnostics(|_, _| {});
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
    let stats = s.decoder().diagnostics().unwrap();
    assert_eq!(stats.decoded_outputs, 3);
    assert_eq!(stats.dropped_obsolete, 2);
    assert_eq!(stats.readbacks_completed, 2);
    assert_eq!(stats.materialized, 1);
    assert_eq!(stats.bytes_materialized, 24);
}

#[wasm_bindgen_test]
async fn drop_closes_pending_and_rejects_active_completion_before_materialization() {
    let mut s = Scenario::new(true);
    let summaries = Rc::new(RefCell::new(Vec::new()));
    let reports = Rc::clone(&summaries);
    s.decoder().enable_diagnostics(move |stats, final_report| {
        if final_report {
            reports.borrow_mut().push(stats);
        }
    });
    s.emit(1);
    drain().await.unwrap();
    s.emit(2);
    s.decoder.take();
    let final_stats = summaries.borrow()[0];
    assert_eq!(final_stats.decoded_outputs, 2);
    assert_eq!(final_stats.dropped_obsolete, 2);
    assert_eq!(final_stats.readbacks_started, 1);
    assert_eq!(final_stats.readbacks_completed, 0);
    assert_eq!(s.browser.close_count(), 1);
    assert!(s.browser.closed(1));
    s.browser.settle(0, false).await.unwrap();
    assert!(s.browser.closed(0));
    assert_eq!(s.browser.count(), 1);
    assert_eq!(s.browser.length_reads(0), 0, "no to_vec after drop");
    assert_eq!(s.images(), 0);
    assert!(s.stamps().is_empty());
    assert_eq!(&*summaries.borrow(), &[final_stats]);
}

#[wasm_bindgen_test]
async fn rejected_copy_closes_pending_and_reset_can_resume() {
    let s = Scenario::new(true);
    s.decoder().enable_diagnostics(|_, _| {});
    s.emit(1);
    drain().await.unwrap();
    s.emit(2);
    s.browser.settle(0, true).await.unwrap();
    assert!(s.decoder().failure().is_none());
    assert_eq!(s.browser.format(1), "RGBA");
    s.browser.settle(1, true).await.unwrap();
    assert!(s.browser.closed(0));
    assert!(s.browser.closed(1));
    assert_eq!(s.browser.count(), 2);
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
    assert_eq!(s.browser.count(), 3, "rejection released the active slot");
    s.browser.settle(2, false).await.unwrap();
    assert_eq!(s.stamps(), [3]);
    assert!(s.decoder().failure().is_none());
    let stats = s.decoder().diagnostics().unwrap();
    assert_eq!(stats.failed_frames, 1);
    assert_eq!(stats.dropped_obsolete, 1);
    assert_eq!(stats.materialized, 1);
    assert_eq!(stats.readbacks_completed, 3);
}

#[wasm_bindgen_test]
async fn bgra_rejection_recovers_same_frame_and_caches_rgba() {
    let s = Scenario::new(true);
    s.decoder().enable_diagnostics(|_, _| {});
    s.emit(1);
    drain().await.unwrap();
    assert_eq!(s.browser.format(0), "BGRA");
    s.browser.settle(0, true).await.unwrap();
    assert!(s.decoder().failure().is_none());
    assert!(s.stamps().is_empty(), "no output before fallback settles");
    assert!(!s.browser.closed(0));
    assert_eq!(s.browser.format(1), "RGBA");
    assert!(s.browser.reused(0, 1));
    s.browser.settle(1, false).await.unwrap();
    assert_eq!(s.stamps(), [1]);
    assert_eq!(
        &s.published.borrow()[0].image.0[0].buffer().as_raw()[..4],
        &[80, 40, 0, 255]
    );
    s.emit(2);
    drain().await.unwrap();
    assert_eq!(s.browser.format(2), "RGBA");
    s.browser.settle(2, false).await.unwrap();
    assert_eq!(s.stamps(), [1, 2]);
    let stats = s.decoder().diagnostics().unwrap();
    assert_eq!(stats.bgra, 1);
    assert_eq!(stats.rgba, 2);
    assert_eq!(stats.copy_fallbacks, 1);
    assert_eq!(stats.readbacks_started, 3);
    assert_eq!(stats.readbacks_completed, 3);
    assert_eq!(stats.bytes_requested, 72);
    assert_eq!(stats.bytes_materialized, 48);
}

#[wasm_bindgen_test]
async fn reset_during_rgba_retry_cancels_publication() {
    let s = Scenario::new(true);
    s.decoder().enable_diagnostics(|_, _| {});
    s.emit(1);
    drain().await.unwrap();
    s.browser.settle(0, true).await.unwrap();
    s.decoder().reset();
    s.emit(2);
    assert_eq!(s.browser.count(), 2);
    s.browser.settle(1, false).await.unwrap();
    assert_eq!(s.images(), 0);
    assert!(s.stamps().is_empty());
    assert_eq!(s.browser.format(2), "RGBA");
    s.browser.settle(2, false).await.unwrap();
    assert_eq!(s.stamps(), [2]);
    assert!(s.decoder().failure().is_none());
    let stats = s.decoder().diagnostics().unwrap();
    assert_eq!(stats.copy_fallbacks, 1);
    assert_eq!(stats.readbacks_started, 3);
    assert_eq!(stats.readbacks_completed, 3);
    assert_eq!(stats.dropped_obsolete, 1);
    assert_eq!(stats.materialized, 1);
}

#[wasm_bindgen_test]
async fn bgra_allocation_rejection_uses_rgba_before_copy() {
    let s = Scenario::new(true);
    s.decoder().enable_diagnostics(|_, _| {});
    s.browser.reject_allocation();
    s.emit(1);
    drain().await.unwrap();
    assert_eq!(s.browser.count(), 1);
    assert_eq!(s.browser.format(0), "RGBA");
    s.browser.settle(0, false).await.unwrap();
    assert_eq!(s.stamps(), [1]);
    assert!(s.decoder().failure().is_none());
    let stats = s.decoder().diagnostics().unwrap();
    assert_eq!(stats.allocation_fallbacks, 1);
    assert_eq!(stats.copy_fallbacks, 0);
    assert_eq!(stats.bgra, 0);
    assert_eq!(stats.rgba, 1);
}

#[wasm_bindgen_test]
async fn real_copy_colors_odd_dimensions_all_turns_and_older_formats() {
    for mode in 0..3 {
        let s = Scenario::new(true);
        s.decoder().enable_diagnostics(|_, _| {});
        match mode {
            1 => s.browser.rgba_only(),
            2 => s.browser.ignore_format(),
            _ => {}
        }
        for orientation in 0..4 {
            let stamp = orientation as i32 + 1;
            feed(s.decoder(), stamp, orientation);
            s.browser.emit(stamp, 3, 5);
            drain().await.unwrap();
            assert_eq!(
                s.browser.format(orientation as u32),
                if mode == 0 && orientation == 0 {
                    "BGRA"
                } else {
                    "RGBA"
                }
            );
            assert_eq!(s.stamps().len(), orientation as usize);
            s.browser.settle(orientation as u32, false).await.unwrap();
            let pictures = s.published.borrow();
            let pixels = pictures.last().unwrap().image.0[0].buffer();
            let (dw, dh) = if orientation % 2 == 0 { (3, 5) } else { (5, 3) };
            assert_eq!(pixels.dimensions(), (dw, dh));
            for y in 0..5 {
                for x in 0..3 {
                    let (dx, dy) = match orientation {
                        0 => (x, y),
                        1 => (y, 2 - x),
                        2 => (2 - x, 4 - y),
                        _ => (4 - y, x),
                    };
                    assert_eq!(pixels.get_pixel(dx, dy).0, [80, 40, (y * 3 + x) as u8, 255]);
                }
            }
            assert!(s.decoder().failure().is_none());
        }
        let stats = s.decoder().diagnostics().unwrap();
        assert_eq!(stats.probe_fallbacks, u64::from(mode != 0));
        assert_eq!(stats.bgra, u64::from(mode == 0));
        assert_eq!(stats.rgba, if mode == 0 { 3 } else { 4 });
    }
}

#[wasm_bindgen_test]
async fn old_generation_rejection_does_not_fail_the_reset_decoder() {
    let s = Scenario::new(true);
    s.decoder().enable_diagnostics(|_, _| {});
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
    let stats = s.decoder().diagnostics().unwrap();
    assert_eq!(stats.failed_frames, 0);
    assert_eq!(stats.dropped_obsolete, 1);
}

#[wasm_bindgen_test]
async fn attachment_overflow_counts_outputs_without_starting_extra_readbacks() {
    let s = Scenario::new(false);
    s.decoder().enable_diagnostics(|_, _| {});
    for stamp in 1..=9 {
        s.emit(stamp);
    }
    drain().await.unwrap();
    assert_eq!(s.browser.count(), 8);
    assert!(s.decoder().take_refusal());
    for index in 0..8 {
        s.browser.settle(index, false).await.unwrap();
    }
    let stats = s.decoder().diagnostics().unwrap();
    assert_eq!(stats.decoded_outputs, 9);
    assert_eq!(stats.dropped_pending, 1);
    assert_eq!(stats.readbacks_started, 8);
    assert_eq!(stats.readbacks_completed, 8);
    assert_eq!(stats.materialized, 8);
    assert_eq!(stats.bytes_requested, 192);
}

#[wasm_bindgen_test]
async fn sink_can_read_decoder_getters_with_diagnostics_enabled() {
    let browser = ControlledDecoder::new();
    let owner = Rc::new(RefCell::new(None::<Decoder>));
    let weak = Rc::downgrade(&owner);
    let observed = Rc::new(RefCell::new(None));
    let result = Rc::clone(&observed);
    browser.install(false);
    let decoder = decoder(Some(Rc::new(move |_| {
        let owner = weak.upgrade().unwrap();
        let held = owner.borrow();
        let decoder = held.as_ref().unwrap();
        assert!(decoder.failure().is_none());
        assert!(decoder.newest().is_some());
        *result.borrow_mut() = decoder.diagnostics();
    })))
    .unwrap();
    browser.restore();
    decoder.enable_diagnostics(|_, _| {});
    *owner.borrow_mut() = Some(decoder);
    browser.emit(1, 3, 2);
    drain().await.unwrap();
    browser.settle(0, false).await.unwrap();
    assert_eq!(observed.borrow().unwrap().materialized, 1);
    owner.borrow_mut().take();
    browser.cleanup();
}

#[wasm_bindgen_test]
async fn attachment_stale_completion_skips_to_vec_and_image_creation() {
    let s = Scenario::new(false);
    s.decoder().enable_diagnostics(|_, _| {});
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
    let stats = s.decoder().diagnostics().unwrap();
    assert_eq!(stats.readbacks_completed, 2);
    assert_eq!(stats.dropped_obsolete, 1);
    assert_eq!(stats.bytes_materialized, 24);
}

#[wasm_bindgen_test]
async fn diagnostics_are_per_decoder_and_disabled_by_default() {
    let first = Scenario::new(true);
    let second = Scenario::new(true);
    let disabled = Scenario::new(true);
    first.decoder().enable_diagnostics(|_, _| {});
    second.decoder().enable_diagnostics(|_, _| {});
    first.emit(1);
    second.browser.emit(1, 4, 3);
    disabled.emit(1);
    drain().await.unwrap();
    first.browser.settle(0, false).await.unwrap();
    second.browser.settle(0, false).await.unwrap();
    disabled.decoder().reset();
    disabled.browser.settle(0, false).await.unwrap();
    first.browser.emit(2, 2, 5);
    drain().await.unwrap();
    first.browser.settle(1, false).await.unwrap();
    let a = first.decoder().diagnostics().unwrap();
    let b = second.decoder().diagnostics().unwrap();
    assert_eq!(a.decoded_outputs, 2);
    assert_eq!(a.bytes_requested, 64);
    assert_eq!(a.bytes_materialized, 64);
    assert_eq!(a.latest_dimensions, Some((2, 5)));
    assert_eq!(a.min_dimensions, Some((2, 2)));
    assert_eq!(a.max_dimensions, Some((3, 5)));
    assert_eq!(b.decoded_outputs, 1);
    assert_eq!(b.bytes_materialized, 48);
    assert_eq!(b.min_dimensions, Some((4, 3)));
    assert_eq!(b.max_dimensions, Some((4, 3)));
    assert!(disabled.decoder().diagnostics().is_none());
    assert!(disabled.decoder().failure().is_none());
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
