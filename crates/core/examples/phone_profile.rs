//! Synthetic allocation probe. Run each cold sample in a new process.
// Machine-readable output is the probe interface.
#![allow(clippy::print_stdout)]
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use wacore::time::Instant;
struct Count;
static TRACK: AtomicBool = AtomicBool::new(true);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Count {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() && TRACK.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            BYTES.fetch_add(layout.size(), Relaxed);
            let live = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
            PEAK.fetch_max(live, Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if TRACK.load(Relaxed) {
            LIVE.fetch_sub(layout.size(), Relaxed);
        }
        unsafe { System.dealloc(p, layout) };
    }
    unsafe fn realloc(&self, p: *mut u8, old: Layout, size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, old, size) };
        if !q.is_null() && TRACK.load(Relaxed) {
            ALLOCS.fetch_add(1, Relaxed);
            BYTES.fetch_add(size, Relaxed);
            let live = LIVE.fetch_add(size, Relaxed) + size;
            LIVE.fetch_sub(old.size(), Relaxed);
            PEAK.fetch_max(live - old.size(), Relaxed);
        }
        q
    }
}
#[global_allocator]
static ALLOCATOR: Count = Count;
fn sample(tag: &str, n: usize, left: bool) {
    use oxidezap_core::{GroupMembership, MembershipAction, NoticeParticipant};
    let actor = NoticeParticipant {
        jid: "12025550123@s.whatsapp.net".into(),
        phone_jid: None,
        lid: None,
        offered_name: Some("Alex".into()),
        label: None,
        is_self: false,
        canonical_jid: None,
    };
    let mut target = actor.clone();
    target.offered_name = None;
    let event = GroupMembership {
        action: MembershipAction::Remove,
        actor: Some(actor),
        participants: vec![target],
    };
    let before = [
        ALLOCS.load(Relaxed),
        BYTES.load(Relaxed),
        LIVE.load(Relaxed),
    ];
    PEAK.store(before[2], Relaxed);
    let start = Instant::now();
    for _ in 0..n {
        if left {
            assert_eq!(std::hint::black_box(event.summary()), "Alex left");
        } else {
            std::hint::black_box(oxidezap_core::format_phone_jid(std::hint::black_box(
                "12025550123@s.whatsapp.net",
            )));
        }
    }
    let elapsed = start.elapsed();
    let after = [
        ALLOCS.load(Relaxed),
        BYTES.load(Relaxed),
        LIVE.load(Relaxed),
        PEAK.load(Relaxed),
    ];
    println!(
        "{tag} n={n} ns={} allocs={} bytes={} retained={} peak_delta={}",
        elapsed.as_nanos(),
        after[0] - before[0],
        after[1] - before[1],
        after[2] - before[2],
        after[3] - before[2]
    );
}
fn main() {
    let left = std::env::args().any(|arg| arg == "left");
    if std::env::args().any(|arg| arg == "--timing") {
        TRACK.store(false, Relaxed);
    }
    sample("cold", 1, left);
    sample("warm", 10000, left);
}
