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
    if std::env::args().any(|arg| arg == "--representative") {
        // Synthetic inputs for distinct formatting paths, not an exhaustive
        // corpus or a claim that these numbers belong to real subscribers.
        for raw in [
            "12025550123@s.whatsapp.net",   // NANPA fictional-use range
            "18005550123@s.whatsapp.net",   // toll-free format
            "442079460000@s.whatsapp.net",  // Ofcom drama landline
            "447700900123@s.whatsapp.net",  // Ofcom drama mobile
            "61491570156@s.whatsapp.net",   // Australian drama mobile
            "390600000000@s.whatsapp.net",  // retained Italian leading zero
            "552120000000@s.whatsapp.net",  // Brazilian fixed-line length
            "5521900000000@s.whatsapp.net", // Brazilian mobile length
            "49300000000@s.whatsapp.net",   // German variable lengths
            "4930000000000@s.whatsapp.net",
            "33100000000@s.whatsapp.net",   // French grouping
            "911100000000@s.whatsapp.net",  // Indian grouping
            "81300000000@s.whatsapp.net",   // Japanese grouping
            "74950000000@s.whatsapp.net",   // shared +7, Russia
            "77000000000@s.whatsapp.net",   // shared +7, Kazakhstan
            "80010000000@s.whatsapp.net",   // non-geographic calling code
            "9990000000@s.whatsapp.net",    // invalid calling-code fallback
            "1202555@s.whatsapp.net",       // short national number
            "12025550123:7@s.whatsapp.net", // device normalization
            "12025550123@c.us",             // legacy PN family
            "111000011112222@lid",          // no telephone semantics
            "012025550123@s.whatsapp.net",  // disallowed leading zero
            "١٢٠٢٥٥٥٠١٢٣@s.whatsapp.net",   // non-ASCII JID user
            "broken",
        ] {
            println!("{raw}\t{:?}", oxidezap_core::format_phone_jid(raw));
        }
        return;
    }
    if std::env::args().any(|arg| arg == "--equivalence") {
        // Synthetic country-code candidates and deliberately repetitive NSNs.
        // Include invalid codes and lengths. Nothing comes from an account.
        for code in 1..1000 {
            for len in 4..=12 {
                for fill in ['0', '9'] {
                    let digits = format!("{code}{}", fill.to_string().repeat(len));
                    let jid = format!("{digits}@s.whatsapp.net");
                    let result = oxidezap_core::format_phone_jid(&jid);
                    assert_eq!(
                        result,
                        oxidezap_core::format_phone_jid(&format!("{digits}:7@s.whatsapp.net"))
                    );
                    println!("{digits}\t{result:?}");
                }
            }
        }
        return;
    }
    let left = std::env::args().any(|arg| arg == "left");
    if std::env::args().any(|arg| arg == "--timing") {
        TRACK.store(false, Relaxed);
    }
    sample("cold", 1, left);
    sample("warm", 10000, left);
}
