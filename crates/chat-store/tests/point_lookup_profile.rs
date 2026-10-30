//! Linux release workload for contact reads and nested identity resolution.
//! Synthetic setup and warmup are excluded. Rust allocation traffic is measured
//! separately from timing; SQLite's C allocator is outside GlobalAlloc.
#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)]
// The profiling allocator and Linux CPU clock wrap their documented FFI only.
#![allow(unsafe_code)]

mod common;
use common::*;
use portable_atomic::AtomicU64;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, Ordering};

static COUNT: AtomicBool = AtomicBool::new(false);
static CALLS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
struct Allocator;

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
fn allocated(size: usize) {
    let live = LIVE.fetch_add(size as u64, Ordering::Relaxed) + size as u64;
    if COUNT.load(Ordering::Relaxed) {
        CALLS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(size as u64, Ordering::Relaxed);
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            allocated(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            allocated(layout.size());
        }
        p
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, size) };
        if !p.is_null() {
            LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
            allocated(size);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn cpu_ns() -> u64 {
    #[repr(C)]
    struct Timespec {
        seconds: std::ffi::c_long,
        nanos: std::ffi::c_long,
    }
    unsafe extern "C" {
        fn clock_gettime(clock: std::ffi::c_int, time: *mut Timespec) -> std::ffi::c_int;
    }
    let mut time = Timespec {
        seconds: 0,
        nanos: 0,
    };
    // Linux CLOCK_PROCESS_CPUTIME_ID includes all pool/runtime threads.
    assert_eq!(unsafe { clock_gettime(2, &mut time) }, 0);
    time.seconds as u64 * 1_000_000_000 + time.nanos as u64
}

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

const CHATS: usize = 48;

#[tokio::test]
#[ignore = "Linux release synthetic point lookup measurement"]
async fn point_lookup_workload() {
    let case = std::env::var("POINT_CASE").unwrap_or_else(|_| "contact".into());
    let iterations: usize = std::env::var("POINT_ITERATIONS")
        .unwrap_or_else(|_| "200".into())
        .parse()
        .unwrap();
    let allocations = std::env::var("POINT_ALLOC").is_ok_and(|v| v == "1");
    let (db, store) = test_store().await;
    db.database().provision_device(2).await.unwrap();
    db.shared().run(|conn| {
        diesel::sql_query("WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<47) INSERT INTO contacts(device_id,jid,full_name) SELECT 1,printf('55990000%04d@s.whatsapp.net',i),'synthetic' FROM n").execute(conn).map_err(db_err)?;
        diesel::sql_query("INSERT INTO contacts SELECT 2,jid,'other', 'other',NULL,NULL FROM contacts WHERE device_id=1").execute(conn).map_err(db_err)?;
        diesel::sql_query("WITH RECURSIVE n(i) AS (VALUES(0) UNION ALL SELECT i+1 FROM n WHERE i<47) INSERT INTO lid_pn_mapping(device_id,lid,phone_number,created_at,updated_at,learning_source) SELECT 1,printf('77000000%04d',i),printf('55990000%04d',i),1,1,'synthetic' FROM n").execute(conn).map_err(db_err)?;
        diesel::sql_query("INSERT INTO lid_pn_mapping(device_id,lid,phone_number,created_at,updated_at,learning_source) SELECT 2,lid,'999',1,1,'synthetic' FROM lid_pn_mapping WHERE device_id=1").execute(conn).map_err(db_err)?;
        Ok(())
    }).await.unwrap();
    let chats: Vec<Jid> = (0..CHATS)
        .map(|i| {
            jid(&if case == "lid" {
                format!("77000000{i:04}@lid")
            } else {
                format!("55990000{i:04}@s.whatsapp.net")
            })
        })
        .collect();
    let missing = jid("559999999999@s.whatsapp.net");
    // Include present/absent contact reads. LID reaction reads exercise the
    // nested identity lookup and its public pool/decoding path with no rows.
    for _ in 0..3 {
        for chat in &chats {
            if case == "contact" {
                assert_eq!(
                    store
                        .contact(chat)
                        .await
                        .unwrap()
                        .unwrap()
                        .full_name
                        .as_deref(),
                    Some("synthetic")
                );
                assert!(store.contact(&missing).await.unwrap().is_none());
            } else {
                assert!(
                    store
                        .reactions_for(chat, vec!["synthetic-id".into()])
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }
    let rss_before = rss_kib();
    CALLS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    let live_before = LIVE.load(Ordering::Relaxed);
    PEAK.store(live_before, Ordering::Relaxed);
    let cpu_before = cpu_ns();
    COUNT.store(allocations, Ordering::Relaxed);
    let start = wacore::time::Instant::now();
    for _ in 0..iterations {
        for chat in &chats {
            if case == "contact" {
                assert_eq!(
                    store
                        .contact(chat)
                        .await
                        .unwrap()
                        .unwrap()
                        .full_name
                        .as_deref(),
                    Some("synthetic")
                );
                assert!(store.contact(&missing).await.unwrap().is_none());
            } else {
                assert!(
                    store
                        .reactions_for(chat, vec!["synthetic-id".into()])
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }
    let elapsed_ns = start.elapsed().as_nanos();
    COUNT.store(false, Ordering::Relaxed);
    let cpu_ns = cpu_ns() - cpu_before;
    let calls = CALLS.load(Ordering::Relaxed);
    let bytes = BYTES.load(Ordering::Relaxed);
    let live_after = LIVE.load(Ordering::Relaxed);
    let peak = PEAK.load(Ordering::Relaxed);
    let rss_after = rss_kib();
    println!(
        "POINT rust_live_before={live_before} rust_live_after={live_after} rust_peak={peak} elapsed_ns={elapsed_ns} cpu_ns={cpu_ns} alloc_calls={calls} alloc_bytes={bytes} rss_before_kib={rss_before} rss_after_kib={rss_after} rows=48 checksum=0 quotes=0 reactions=0"
    );
}
