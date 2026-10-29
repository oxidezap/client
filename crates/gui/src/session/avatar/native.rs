//! Native implementation of persistent avatar storage.
//!
//! Shares the filesystem with the daemon directly under the existing
//! media cache directory (`oxidezap_ipc::media_path`). Reads and writes run off the UI
//! thread via `smol::unblock`.

use std::sync::RwLock;

static ACTIVE_ACCOUNT: RwLock<Option<oxidezap_core::AccountId>> = RwLock::new(None);

#[cfg(test)]
pub(super) static PERSISTENT_READS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
pub(super) static DECODE_COMPLETED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
pub(super) static DECODE_PERFORMED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn set_active_account(account: Option<oxidezap_core::AccountId>) {
    if let Ok(mut lock) = ACTIVE_ACCOUNT.write() {
        *lock = account;
    }
}

pub fn active_account() -> Option<oxidezap_core::AccountId> {
    ACTIVE_ACCOUNT.read().ok().and_then(|lock| *lock)
}

pub fn rotate_account_scope() -> Option<oxidezap_core::AccountId> {
    let old = active_account();
    set_active_account(None);
    old
}

#[allow(dead_code)]
pub fn resolve_account(key: &str) -> Option<oxidezap_core::AccountId> {
    oxidezap_ipc::account_id_of(key).or_else(active_account)
}

pub async fn read_persistent(key: &str) -> Option<Vec<u8>> {
    let path = oxidezap_ipc::media_path(key)?;
    #[cfg(test)]
    PERSISTENT_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    smol::unblock(move || std::fs::read(path).ok()).await
}

// A restore can read dozens of pictures together. Decoding them on smol's
// single default executor thread prevents other futures from making progress;
// unbounded blocking jobs would instead compete with rendering for every CPU
// and retain one full-size decoded allocation per picture.
struct DecodeJob {
    priority: super::DecodePriority,
    run: Box<dyn FnOnce() + Send>,
}
struct DecodeQueue {
    jobs: std::sync::Mutex<std::collections::VecDeque<DecodeJob>>,
    ready: std::sync::Condvar,
    slots: tokio::sync::Semaphore,
}
static DECODE_JOBS: std::sync::LazyLock<std::sync::Arc<DecodeQueue>> =
    std::sync::LazyLock::new(|| {
        let queue = std::sync::Arc::new(DecodeQueue {
            jobs: std::sync::Mutex::new(std::collections::VecDeque::new()),
            ready: std::sync::Condvar::new(),
            slots: tokio::sync::Semaphore::const_new(super::MAX_PERSISTENT_READS),
        });
        for index in 0..2 {
            let queue = queue.clone();
            std::thread::Builder::new()
                .name(format!("avatar-decode-{index}"))
                .spawn(move || {
                    loop {
                        let job = {
                            let mut jobs = queue.jobs.lock().unwrap_or_else(|e| e.into_inner());
                            while jobs.is_empty() {
                                jobs = queue.ready.wait(jobs).unwrap_or_else(|e| e.into_inner());
                            }
                            // Read priority when selecting work: scrolling can promote
                            // an existing reservation without rereading its file.
                            let index = jobs
                                .iter()
                                .position(|job| {
                                    job.priority.load(portable_atomic::Ordering::Relaxed)
                                })
                                .unwrap_or(0);
                            jobs.remove(index).expect("nonempty decode queue")
                        };
                        (job.run)();
                    }
                })
                .expect("start avatar decoder");
        }
        queue
    });

pub(super) async fn decode_avatar(
    bytes: Vec<u8>,
    priority: super::DecodePriority,
    current: impl FnOnce() -> bool + Send + 'static,
) -> Option<(std::sync::Arc<gpui::RenderImage>, u64)> {
    decode_work(priority, move || {
        if !current() {
            return None;
        }
        #[cfg(test)]
        DECODE_PERFORMED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::session::media::decode_avatar(&bytes)
    })
    .await
}

pub(super) async fn decode_work<T: Send + 'static>(
    priority: super::DecodePriority,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    // Count running and queued jobs together. Producers await admission
    // without blocking the async executor.
    let permit = DECODE_JOBS
        .slots
        .acquire()
        .await
        .expect("decode slots never close");
    let (send, receive) = smol::channel::bounded(1);
    DECODE_JOBS
        .jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(DecodeJob {
            priority,
            run: Box::new(move || {
                let _permit = permit;
                let _ = send.try_send(work());
            }),
        });
    DECODE_JOBS.ready.notify_one();
    let result = receive
        .recv()
        .await
        .expect("avatar decoder returns a result");
    #[cfg(test)]
    DECODE_COMPLETED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    result
}

#[cfg(test)]
pub(super) fn queued_decodes() -> usize {
    DECODE_JOBS
        .jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

pub async fn delete_persistent(key: &str) -> Result<(), String> {
    if let Some(path) = oxidezap_ipc::media_path(key) {
        let _ = smol::unblock(move || std::fs::remove_file(path)).await;
    }
    Ok(())
}

#[allow(dead_code)]
pub async fn write_persistent(
    _key: &str,
    _bytes: &[u8],
    _account: Option<oxidezap_core::AccountId>,
    _stamp: u64,
) -> Result<(), String> {
    // Daemon already persists media atomically; no redundant write needed on native.
    Ok(())
}

pub async fn delete_account_storage(
    _account: Option<oxidezap_core::AccountId>,
) -> Result<(), String> {
    // Daemon manages file deletion on account wipe
    Ok(())
}

pub async fn clear_cache_storage(_account: Option<oxidezap_core::AccountId>) -> Result<(), String> {
    // Daemon manages file deletion on media cache clear
    Ok(())
}

#[allow(dead_code)]
pub async fn avatar_cache_usage(_account: oxidezap_core::AccountId) -> (u64, u64) {
    (0, 0)
}

pub async fn purge_legacy_caches() {
    // Native does not use browser Cache Storage
}

pub fn spawn_task(fut: impl std::future::Future<Output = ()> + Send + 'static) {
    smol::spawn(fut).detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn decoding_yields_the_executor_and_bounds_cpu_jobs() {
        let release = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (started, receiving) = std::sync::mpsc::channel();
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let release = release.clone();
            let active = active.clone();
            let peak = peak.clone();
            let started = started.clone();
            tasks.push(smol::spawn(decode_work(
                super::super::decode_priority(super::super::DemandPriority::High),
                move || {
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(count, Ordering::SeqCst);
                    started.send(()).unwrap();
                    while !release.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    active.fetch_sub(1, Ordering::SeqCst);
                },
            )));
        }
        let both_started = receiving.recv_timeout(Duration::from_secs(5)).is_ok()
            && receiving.recv_timeout(Duration::from_secs(5)).is_ok();
        let pulse = smol::block_on(smol::future::or(
            async { smol::spawn(async { true }).await },
            async {
                smol::Timer::after(Duration::from_secs(1)).await;
                false
            },
        ));
        let third_started_early = receiving.try_recv().is_ok();
        release.store(true, Ordering::SeqCst);
        smol::block_on(async {
            for task in tasks {
                task.await;
            }
        });
        assert!(both_started, "blocking workers must run concurrently");
        assert!(pulse, "decoding must leave the async executor runnable");
        assert!(!third_started_early, "the third job must wait for a slot");
        assert_eq!(peak.load(Ordering::SeqCst), 2);

        // Keep one worker occupied while the other drains a low-priority
        // request queued before a visible one. Visible work must overtake it.
        let (started, receiving) = std::sync::mpsc::channel();
        let mut blockers = Vec::new();
        let mut releases = Vec::new();
        for _ in 0..2 {
            let release = Arc::new(AtomicBool::new(false));
            releases.push(release.clone());
            let started = started.clone();
            blockers.push(smol::spawn(decode_work(
                super::super::decode_priority(super::super::DemandPriority::High),
                move || {
                    started.send(()).unwrap();
                    while !release.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                },
            )));
        }
        for _ in 0..2 {
            receiving.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let (order, received) = std::sync::mpsc::channel();
        let low_order = order.clone();
        let low = smol::spawn(decode_work(
            super::super::decode_priority(super::super::DemandPriority::Low),
            move || low_order.send("overscan").unwrap(),
        ));
        let promoted = super::super::decode_priority(super::super::DemandPriority::Low);
        let high = smol::spawn(decode_work(promoted.clone(), move || {
            order.send("visible").unwrap()
        }));
        let deadline = wacore::time::Instant::now() + Duration::from_secs(5);
        while queued_decodes() < 2 {
            assert!(wacore::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        promoted.store(true, portable_atomic::Ordering::Relaxed);
        releases[0].store(true, Ordering::SeqCst);
        let first = received.recv_timeout(Duration::from_secs(5));
        let second = received.recv_timeout(Duration::from_secs(5));
        releases[1].store(true, Ordering::SeqCst);
        smol::block_on(async {
            low.await;
            high.await;
            for blocker in blockers {
                blocker.await;
            }
        });
        assert_eq!(first.unwrap(), "visible");
        assert_eq!(second.unwrap(), "overscan");
    }
}
