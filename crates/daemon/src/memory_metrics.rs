//! Requested Rust heap bytes, separate from the process's resident pages.
//! Opt-in test instrumentation, never linked into a production allocator.

// Machine-readable output is the test harness's measurement interface.
#![allow(clippy::print_stdout)]

use portable_atomic::{AtomicUsize, Ordering};
use std::alloc::{GlobalAlloc, Layout, System};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static REQUESTED: AtomicUsize = AtomicUsize::new(0);

fn added(size: usize) {
    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    REQUESTED.fetch_add(size, Ordering::Relaxed);
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: pass the allocation contract through to System unchanged.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            added(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: pass the allocation contract through to System unchanged.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            added(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller provides the original pointer and layout.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller provides a live pointer/layout and a valid size.
        let next = unsafe { System.realloc(ptr, layout, size) };
        if !next.is_null() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            REQUESTED.fetch_add(size, Ordering::Relaxed);
            let live = if size >= layout.size() {
                LIVE.fetch_add(size - layout.size(), Ordering::Relaxed) + size - layout.size()
            } else {
                LIVE.fetch_sub(layout.size() - size, Ordering::Relaxed) - (layout.size() - size)
            };
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

pub(crate) fn begin() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    println!("OXIDEZAP_ALLOCATOR counting");
}

pub(crate) fn checkpoint(stage: &str) {
    if std::env::var_os("OXIDEZAP_MEMORY_REPORT").is_none() {
        return;
    }
    // Read counters before the sampling/formatting allocations. Samples
    // themselves are counted in subsequent checkpoints on both revisions.
    let live = LIVE.load(Ordering::Relaxed);
    let peak = PEAK.load(Ordering::Relaxed);
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let requested = REQUESTED.load(Ordering::Relaxed);
    let process = process::snapshot();
    println!(
        "OXIDEZAP_MEMORY {}",
        serde_json::json!({
            "stage": stage, "live_bytes": live, "peak_bytes": peak,
            "allocations": allocations, "requested_bytes": requested,
            "process_kib": process,
        })
    );
}

#[cfg(target_os = "linux")]
mod process {
    pub(super) fn snapshot() -> serde_json::Value {
        let mut fields = serde_json::Map::new();
        for path in ["/proc/self/smaps_rollup", "/proc/self/status"] {
            if let Ok(contents) = std::fs::read_to_string(path) {
                for line in contents.lines() {
                    let Some((name, value)) = line.split_once(':') else {
                        continue;
                    };
                    if matches!(
                        name,
                        "Rss"
                            | "Pss"
                            | "Pss_Anon"
                            | "Pss_File"
                            | "Pss_Shmem"
                            | "Anonymous"
                            | "Private_Clean"
                            | "Private_Dirty"
                            | "Shared_Clean"
                            | "Shared_Dirty"
                            | "Swap"
                            | "VmHWM"
                            | "RssAnon"
                            | "RssFile"
                            | "RssShmem"
                    ) && let Some(number) = value
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse::<u64>().ok())
                    {
                        fields.insert(name.into(), number.into());
                    }
                }
            }
        }
        serde_json::Value::Object(fields)
    }
}

#[cfg(not(target_os = "linux"))]
mod process {
    pub(super) fn snapshot() -> serde_json::Value {
        serde_json::Value::Null
    }
}
