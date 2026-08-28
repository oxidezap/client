//! The page's media: a map, because both ends of the socket are this process.
//!
//! On a desktop the daemon writes a file and the front end opens it, and the
//! two are separate processes that share only a directory. In a page they are
//! one process sharing an address space, so the file *is* the map — there is
//! nothing to serialize it through and nobody to hand a path to.
//!
//! Everything the directory gave for free has to be said here instead: the
//! budget, the sweep, and the fact that a staged upload is not a cache. What
//! it costs is memory rather than disk, which is why the budget is a
//! different number and not the same one.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Result;

use super::{Wipe, is_staged_upload};

/// How much media the page may hold before the oldest is dropped.
///
/// Two orders of magnitude under the daemon's, and for a different reason.
/// The daemon spends disk, which is cheap and outlives it; this is the wasm
/// heap, which is bounded by the module's own maximum and shared with
/// everything the interface is drawing. A cache that spent it would not be a
/// slow page — it would be an allocation failure with no way back.
const CACHE_BUDGET_BYTES: u64 = 48 * 1024 * 1024;

/// One entry, and when it was last useful.
struct Entry {
    bytes: Vec<u8>,
    /// Bumped on every write and every read, so the sweep drops what has gone
    /// longest without being wanted. A clock rather than a timestamp: there
    /// is no time here that a test would not have to fake.
    touched: u64,
    /// Somebody asked for these bytes and has not been handed them yet.
    ///
    /// The same standing a staged upload has, for the same reason: this is
    /// not a copy of something that can be fetched again *in time to matter*.
    /// It is the delivery of a request already answered `Ok`, and the reader
    /// is on its way. Dropping it makes a download that WhatsApp completed
    /// report a failure — so it is exempt from the sweep until it is read,
    /// and reading it is what takes it out of the map altogether.
    ///
    /// Exempting it from *eviction* only. It still counts toward the budget,
    /// so it presses older entries out rather than raising the ceiling.
    pinned: bool,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    clock: u64,
    held: u64,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut Cache) -> T) -> T {
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Cache::default))
}

/// Write `bytes` under `key`, unless they are already there.
///
/// Returns the key, so a caller can hand it straight to the peer.
///
/// # Errors
///
/// Never, here. A map does not fail to be written to, and the signature is
/// the desktop's, where a disk does.
pub fn put(key: &str, bytes: &[u8]) -> Result<String> {
    store(key, bytes, true)
}

/// Cache `bytes` under `key`, droppable from the moment they land.
///
/// For the eager copy of an inbound message's media, which nobody is waiting
/// on and which can be fetched again on demand. See [`super::put_since`],
/// whose whole subject is that this write is the one allowed to lose.
pub fn put_evictable(key: &str, bytes: &[u8]) -> Result<String> {
    store(key, bytes, false)
}

fn store(key: &str, bytes: &[u8], pinned: bool) -> Result<String> {
    with(|cache| {
        // Content-addressed: the same key is the same bytes, so an entry that
        // is already there is already right — and re-storing it would be a
        // second copy of a photo for as long as it took to replace the first.
        if let Some(entry) = cache.entries.get_mut(key) {
            cache.clock += 1;
            entry.touched = cache.clock;
            // A key first cached eagerly and then asked for is now somebody's
            // answer, so it takes the stronger standing. Never the reverse:
            // an eager write must not unpin bytes a reader is coming for.
            entry.pinned |= pinned;
            return Ok(key.to_string());
        }
        cache.clock += 1;
        let touched = cache.clock;
        cache.held += bytes.len() as u64;
        cache.entries.insert(
            key.to_string(),
            Entry {
                bytes: bytes.to_vec(),
                touched,
                pinned,
            },
        );
        sweep(cache);
        Ok(key.to_string())
    })
}

/// Read what is under `key` and remove it.
///
/// For payloads a client staged rather than the daemon cached: their bytes
/// never counted toward the sweep, so nothing else would ever clear them.
pub fn take(key: &str) -> Option<Vec<u8>> {
    with(|cache| {
        let entry = cache.entries.remove(key)?;
        cache.held = cache.held.saturating_sub(entry.bytes.len() as u64);
        Some(entry.bytes)
    })
}

/// Read what is under `key` and leave it there.
///
/// The desktop has no such call: its front end opens the file itself. Here
/// the front end is this process, so this is how a frame's media reaches it.
pub fn read(key: &str) -> Option<Vec<u8>> {
    with(|cache| {
        let entry = cache.entries.get_mut(key)?;
        cache.clock += 1;
        entry.touched = cache.clock;
        Some(entry.bytes.clone())
    })
}

/// Whether `key` is already cached, without reading it.
pub fn has(key: &str) -> bool {
    with(|cache| cache.entries.contains_key(key))
}

/// What the media cache occupies: bytes, and how many entries.
pub fn cache_usage() -> (u64, u64) {
    with(|cache| (cache.held, cache.entries.len() as u64))
}

/// Delete the cached entries this wipe is entitled to.
///
/// # Errors
///
/// Never, for the same reason [`put`] does not.
pub fn wipe(scope: Wipe) -> Result<()> {
    with(|cache| {
        cache.entries.retain(|name, entry| {
            let taken = scope.takes(name);
            if taken {
                cache.held = cache.held.saturating_sub(entry.bytes.len() as u64);
            }
            !taken
        });
    });
    Ok(())
}

/// Drop the least recently wanted entries until the budget is met.
///
/// Staged uploads are never dropped: there is no other copy of one, so
/// reclaiming it turns an unrelated photo into a voice note that fails to
/// send. They are excluded from what is *counted* too — a cache cannot
/// reclaim what it may not touch, and counting it would make the sweep spin
/// against a budget it can never reach.
fn sweep(cache: &mut Cache) {
    let reclaimable: u64 = cache
        .entries
        .iter()
        .filter(|(name, _)| !is_staged_upload(name))
        .map(|(_, entry)| entry.bytes.len() as u64)
        .sum();
    if reclaimable <= CACHE_BUDGET_BYTES {
        return;
    }

    let mut oldest: Vec<(String, u64, u64)> = cache
        .entries
        .iter()
        .filter(|(name, entry)| !is_staged_upload(name) && !entry.pinned)
        .map(|(name, entry)| (name.clone(), entry.touched, entry.bytes.len() as u64))
        .collect();
    oldest.sort_by_key(|(_, touched, _)| *touched);

    let mut over = reclaimable - CACHE_BUDGET_BYTES;
    for (name, _, size) in oldest {
        if over == 0 {
            break;
        }
        cache.entries.remove(&name);
        cache.held = cache.held.saturating_sub(size);
        over = over.saturating_sub(size);
    }
}
