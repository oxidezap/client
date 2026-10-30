use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
    static CALLS: Cell<usize> = const { Cell::new(0) };
}
struct Counting;
fn add(size: usize) {
    if ACTIVE.try_with(Cell::get).unwrap_or(false) {
        LIVE.with(|v| {
            v.set(v.get() + size);
            PEAK.with(|p| p.set(p.get().max(v.get())));
        });
        CALLS.with(|v| v.set(v.get() + 1));
    }
}
fn subtract(size: usize) {
    if ACTIVE.try_with(Cell::get).unwrap_or(false) {
        LIVE.with(|v| v.set(v.get() - size));
    }
}
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: allocation layout forwarded unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            add(layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: allocation layout forwarded unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            add(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        subtract(layout.size());
        // SAFETY: original allocation and layout forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: original allocation and requested size forwarded unchanged.
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
fn sample(f: impl FnOnce() -> oxidezap_wire::ResponseEnvelope) -> (usize, usize, usize) {
    LIVE.with(|v| v.set(0));
    PEAK.with(|v| v.set(0));
    CALLS.with(|v| v.set(0));
    ACTIVE.with(|v| v.set(true));
    let result = std::hint::black_box(f());
    let metrics = (
        PEAK.with(Cell::get),
        LIVE.with(Cell::get),
        CALLS.with(Cell::get),
    );
    drop(result);
    assert_eq!(LIVE.with(Cell::get), 0);
    ACTIVE.with(|v| v.set(false));
    metrics
}

#[path = "support/response_cases.rs"]
mod cases;
#[test]
#[ignore = "release-only requested heap comparison, separate from timing"]
fn response_decode_heap() {
    for count in [1, 100, 1000] {
        for escaped in [false, true] {
            let frame = cases::chats(count, escaped);
            drop(cases::baseline(&frame).unwrap());
            drop(cases::candidate(&frame).unwrap());
            let base = sample(|| cases::baseline(&frame).unwrap());
            let head = sample(|| cases::candidate(&frame).unwrap());
            assert_eq!(base.1, head.1, "retained DTO heap must be unchanged");
            println!(
                "chats={count} escaped={escaped} base_peak={} head_peak={} base_live={} head_live={} base_calls={} head_calls={}",
                base.0, head.0, base.1, head.1, base.2, head.2
            );
        }
    }
}
