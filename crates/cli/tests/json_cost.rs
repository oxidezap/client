//! Synthetic output-only comparison. Run in release with --ignored --nocapture.
//! This excludes IPC, DTO construction, stdout and the connected daemon.
#![allow(dead_code)]

#[path = "../src/output.rs"]
mod output;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::time::Instant;

use oxidezap_wire::dto::MessageDto;
use serde::Serialize;

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

// Count only this thread during the allocation sample. Timing runs separately
// with accounting disabled. Signed balances also tolerate cross-thread frees.
struct Counting;
fn add(bytes: usize) {
    if !ACTIVE.try_with(Cell::get).unwrap_or(false) {
        return;
    }
    let _ = LIVE.try_with(|v| {
        v.set(v.get() + bytes as isize);
        let _ = PEAK.try_with(|p| p.set(p.get().max(v.get())));
    });
    let _ = CALLS.try_with(|v| v.set(v.get() + 1));
}
fn subtract(bytes: usize) {
    if !ACTIVE.try_with(Cell::get).unwrap_or(false) {
        return;
    }
    let _ = LIVE.try_with(|v| v.set(v.get() - bytes as isize));
}
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forward the caller's layout unchanged to System.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            add(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        subtract(layout.size());
        // SAFETY: forward the pointer and its layout unchanged to System.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: forward the caller's allocation and requested size to System.
        let next = unsafe { System.realloc(ptr, layout, size) };
        if !next.is_null() {
            subtract(layout.size());
            add(size);
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn legacy<T: Serialize>(data: &T) -> String {
    let value = serde_json::to_value(data).unwrap_or(serde_json::Value::Null);
    let envelope = serde_json::json!({ "ok": true, "data": value });
    serde_json::to_string_pretty(&envelope).unwrap()
}
fn messages(count: usize, text_len: usize) -> Vec<MessageDto> {
    (0..count)
        .map(|i| MessageDto {
            id: format!("synthetic-{i}"),
            chat_jid: "synthetic@example.invalid".into(),
            sender_jid: "sender@example.invalid".into(),
            text: Some("é\n\"\\".repeat(text_len / 5)),
            kind: "text".into(),
            status: "read".into(),
            ..Default::default()
        })
        .collect()
}

#[test]
fn borrowed_envelope_preserves_json_values() {
    for data in [messages(0, 0), messages(2, 100)] {
        let old: serde_json::Value = serde_json::from_str(&legacy(&data)).unwrap();
        let new: serde_json::Value =
            serde_json::from_str(&output::json_ok(&data).unwrap()).unwrap();
        assert_eq!(old, new);
    }
    let map = serde_json::json!({ "nested": { "empty": [], "null": null, "number": 123 } });
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&legacy(&map)).unwrap(),
        serde_json::from_str::<serde_json::Value>(&output::json_ok(&map).unwrap()).unwrap(),
    );
}

#[test]
fn failed_serialization_keeps_the_null_fallback() {
    struct Failure;
    impl Serialize for Failure {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("synthetic failure"))
        }
    }
    let result: serde_json::Value =
        serde_json::from_str(&output::json_ok(&Failure).unwrap()).unwrap();
    assert_eq!(result["ok"], true);
    assert!(result["data"].is_null());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&legacy(&Failure)).unwrap(),
        result,
    );
}

// Native-only output measurement; the CLI deliberately has no wacore dependency.
#[allow(clippy::disallowed_methods)]
fn sample(mut f: impl FnMut() -> String) -> (u128, usize, usize, usize) {
    ACTIVE.with(|v| v.set(true));
    let live = LIVE.with(Cell::get);
    PEAK.with(|p| p.set(live));
    let calls = CALLS.with(Cell::get);
    let measured = black_box(f());
    let peak = (PEAK.with(Cell::get) - live) as usize;
    let calls = CALLS.with(Cell::get) - calls;
    drop(measured);
    ACTIVE.with(|v| v.set(false));
    let start = Instant::now();
    let mut bytes = 0;
    for _ in 0..10 {
        bytes = black_box(f()).len();
    }
    let nanos = start.elapsed().as_nanos() / 10;
    (nanos, peak, calls, bytes)
}

#[test]
#[ignore = "release-only output cost measurement"]
fn json_output_cost() {
    for (count, text_len) in [(1, 100), (200, 100), (5000, 100), (200, 16000)] {
        let data = messages(count, text_len);
        // Warm both paths, then alternate their order in every paired sample.
        black_box(legacy(&data));
        black_box(output::json_ok(&data).unwrap());
        for round in 0..12 {
            let (base, head) = if round % 2 == 0 {
                (
                    sample(|| legacy(&data)),
                    sample(|| output::json_ok(&data).unwrap()),
                )
            } else {
                let head = sample(|| output::json_ok(&data).unwrap());
                (sample(|| legacy(&data)), head)
            };
            assert_eq!(base.3, head.3);
            println!(
                "rows={count} text={text_len} round={round} base_ns={} head_ns={} base_peak={} head_peak={} base_calls={} head_calls={} bytes={}",
                base.0, head.0, base.1, head.1, base.2, head.2, base.3
            );
        }
    }
}
