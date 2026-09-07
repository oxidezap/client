#![cfg(target_family = "wasm")]

use wasm_bindgen::prelude::*;
use wasm_bindgen_test::wasm_bindgen_test;

#[path = "../diagnostics.rs"]
mod diagnostics;
#[path = "../output.rs"]
mod output;
#[path = "../stats.rs"]
mod stats;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen(inline_js = r#"
export function sharedCopy() {
    const memory = new WebAssembly.Memory({initial: 1, maximum: 1, shared: true});
    const shared = new Float32Array(memory.buffer, 0, 1024);
    shared[0] = 0.25;
    shared[1023] = -0.5;
    const buffer = new AudioBuffer({length: 1024, sampleRate: 48000, numberOfChannels: 1});
    let refused = false;
    try { buffer.copyToChannel(shared, 0); } catch (e) { refused = e instanceof TypeError; }
    if (!refused) throw new Error('shared view was not rejected');
    const owned = new Float32Array(1024);
    owned.set(shared);
    buffer.copyToChannel(owned, 0);
    shared.fill(0);
    const output = buffer.getChannelData(0);
    if (output[0] !== 0.25 || output[1023] !== -0.5) throw new Error('copy lost samples');
    if (owned.buffer instanceof SharedArrayBuffer) throw new Error('destination is shared');
}
export function trackArrays() {
    const Original = globalThis.Float32Array;
    const state = { allocations: 0, restore() { globalThis.Float32Array = Original; } };
    globalThis.Float32Array = new Proxy(Original, {
        construct(target, args) {
            if (typeof args[0] === 'number' || ArrayBuffer.isView(args[0])) state.allocations++;
            return Reflect.construct(target, args);
        }
    });
    return state;
}
export function allocations(state) { return state.allocations; }
export function restore(state) { state.restore(); }
export function audioBuffer() {
    return new AudioBuffer({length: 1024, sampleRate: 48000, numberOfChannels: 1});
}
export function assertShared(memory) {
    if (!(memory.buffer instanceof SharedArrayBuffer)) throw new Error('test wasm memory is not shared');
}
export function growMemory(memory) { memory.grow(1); }
export function trackTimers() {
    const set = globalThis.setTimeout, clear = globalThis.clearTimeout;
    const pending = new Map();
    const state = {
        pending,
        restore() { globalThis.setTimeout = set; globalThis.clearTimeout = clear; },
        fire() {
            for (const [id, callback] of pending) {
                clear(id);
                pending.delete(id);
                callback();
                return;
            }
            throw new Error('no report timer');
        }
    };
    globalThis.setTimeout = function(callback, ms, ...args) {
        const id = set(callback, ms, ...args);
        if (ms === 5000) pending.set(id, callback);
        return id;
    };
    globalThis.clearTimeout = function(id) { pending.delete(id); clear(id); };
    return state;
}
export function pendingTimers(state) { return state.pending.size; }
export function fireTimer(state) { state.fire(); }
"#)]
extern "C" {
    #[wasm_bindgen(js_name = sharedCopy)]
    fn shared_copy();
    #[wasm_bindgen(js_name = trackArrays)]
    fn track_arrays() -> JsValue;
    fn allocations(state: &JsValue) -> u32;
    fn restore(state: &JsValue);
    #[wasm_bindgen(js_name = audioBuffer)]
    fn audio_buffer() -> web_sys::AudioBuffer;
    #[wasm_bindgen(js_name = assertShared)]
    fn assert_shared(memory: &JsValue);
    #[wasm_bindgen(js_name = growMemory)]
    fn grow_memory(memory: &JsValue);
    #[wasm_bindgen(js_name = trackTimers)]
    fn track_timers() -> JsValue;
    #[wasm_bindgen(js_name = pendingTimers)]
    fn pending_timers(state: &JsValue) -> u32;
    #[wasm_bindgen(js_name = fireTimer)]
    fn fire_timer(state: &JsValue);
}

struct Arrays(JsValue);
impl Drop for Arrays {
    fn drop(&mut self) {
        restore(&self.0);
    }
}

#[wasm_bindgen_test]
fn browser_rejects_shared_wasm_view_and_accepts_owned_copy() {
    shared_copy();
}

#[wasm_bindgen_test]
fn legacy_playout_allocates_a_sample_buffer_per_block() {
    let scratch = [0.25f32; 1024];
    let arrays = Arrays(track_arrays());
    for _ in 0..1000 {
        let _block = js_sys::Float32Array::from(&scratch[..]);
    }
    assert_eq!(allocations(&arrays.0), 1000);
}

#[wasm_bindgen_test]
fn production_output_reuses_one_sample_buffer_and_zero_fills_every_block() {
    #[cfg(target_feature = "atomics")]
    assert_shared(&wasm_bindgen::memory());
    let buffer = audio_buffer();
    let mut ring = std::collections::VecDeque::with_capacity(1024);
    let arrays = Arrays(track_arrays());
    let mut output = output::Output::new(1024);
    assert_eq!(allocations(&arrays.0), 1);
    for callback in 0..1000 {
        if callback == 500 {
            grow_memory(&wasm_bindgen::memory());
        }
        let len = match callback % 3 {
            0 => 1024,
            1 => 17,
            _ => 0,
        };
        ring.extend(std::iter::repeat_n(0.25, len));
        output.fill(&mut ring);
        output.write(&buffer).unwrap();
        assert!(ring.is_empty());
        let samples = buffer.get_channel_data(0).unwrap();
        assert!(samples[..len].iter().all(|&sample| sample == 0.25));
        assert!(samples[len..].iter().all(|&sample| sample == 0.0));
    }
    assert_eq!(
        allocations(&arrays.0),
        1,
        "no new sample storage after construction"
    );
}

#[wasm_bindgen_test]
fn output_keeps_surplus_samples_for_the_next_callback() {
    let mut output = output::Output::new(1024);
    let buffer = audio_buffer();
    let mut ring = std::collections::VecDeque::from(vec![0.5; 2048]);
    output.fill(&mut ring);
    output.write(&buffer).unwrap();
    assert_eq!(ring.len(), 1024);
    output.fill(&mut ring);
    output.write(&buffer).unwrap();
    assert!(ring.is_empty());
    assert!(
        buffer
            .get_channel_data(0)
            .unwrap()
            .iter()
            .all(|&sample| sample == 0.5)
    );
}

struct Logger;
thread_local! {
    static LOGS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}
impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        if record.level() <= log::Level::Warn {
            LOGS.with_borrow_mut(|logs| logs.push(record.args().to_string()));
        }
    }
    fn flush(&self) {}
}

fn reset_logs() {
    let _ = log::set_logger(&Logger);
    log::set_max_level(log::LevelFilter::Debug);
    LOGS.with_borrow_mut(Vec::clear);
}

#[wasm_bindgen_test]
fn callback_counters_and_errors_log_only_when_reported() {
    reset_logs();
    let mut diagnostics = diagnostics::Diagnostics::new(true);
    let period = std::time::Duration::from_millis(20);
    for callback in 0..1000 {
        diagnostics.stats.as_mut().unwrap().record(
            if callback % 2 == 0 { 1024 } else { 0 },
            1024,
            period * callback,
            period,
        );
        diagnostics.write_failed(JsValue::from_str("synthetic write failure"));
    }
    LOGS.with_borrow(|logs| assert!(logs.is_empty()));
    diagnostics.report(false);
    LOGS.with_borrow(|logs| {
        assert_eq!(logs.len(), 2);
        assert!(logs[0].contains("synthetic write failure"));
        assert!(logs[1].contains("500 starved blocks in 500 runs, 512000 missing samples"));
        assert!(!logs[1].contains("peer"));
    });
    diagnostics.write_failed(JsValue::from_str("not reported twice"));
    diagnostics.report(true);
    diagnostics.report(true);
    diagnostics.report(false);
    LOGS.with_borrow(|logs| assert_eq!(logs.len(), 3));
    let mut disabled = diagnostics::Diagnostics::new(false);
    assert!(disabled.stats.is_none());
    disabled.report(true);
    LOGS.with_borrow(|logs| assert_eq!(logs.len(), 3));
}

#[wasm_bindgen_test]
async fn reporter_cancels_its_wait_on_completion_and_cancellation() {
    use std::cell::{Cell, RefCell};
    use std::task::Poll;
    use std::time::Duration;

    for cancelled in [false, true] {
        reset_logs();
        let timers = Arrays(track_timers());
        let diagnostics = RefCell::new(diagnostics::Diagnostics::new(true));
        diagnostics.borrow_mut().stats.as_mut().unwrap().record(
            1,
            1024,
            Duration::ZERO,
            Duration::from_millis(20),
        );
        let ended = Cell::new(false);
        let ending = std::future::poll_fn(|_| {
            if ended.get() {
                Poll::Ready(7)
            } else {
                Poll::Pending
            }
        });
        let mut reporting = Box::pin(diagnostics::report_until(ending, &diagnostics));
        assert_eq!(futures_lite::future::poll_once(&mut reporting).await, None);
        assert_eq!(pending_timers(&timers.0), 1);
        fire_timer(&timers.0);
        assert_eq!(futures_lite::future::poll_once(&mut reporting).await, None);
        LOGS.with_borrow(|logs| assert_eq!(logs.len(), 1));
        assert_eq!(pending_timers(&timers.0), 1);
        if !cancelled {
            ended.set(true);
            assert_eq!(
                futures_lite::future::poll_once(&mut reporting).await,
                Some(7)
            );
        }
        drop(reporting);
        assert_eq!(pending_timers(&timers.0), 0);
        diagnostics.borrow_mut().report(true);
        diagnostics.borrow_mut().report(false);
        LOGS.with_borrow(|logs| {
            assert_eq!(logs.len(), 2);
            assert!(logs[1].contains("final total"));
        });
    }
}
