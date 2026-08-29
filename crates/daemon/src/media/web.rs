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
use std::sync::{Arc, Mutex};

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
    /// Shared, because the front end is this process and one payload can be
    /// named by a hundred rows of one frame. Handing each of them a copy is
    /// how a 10 MiB photo becomes a gigabyte.
    bytes: Arc<Vec<u8>>,
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
    store(key, bytes.to_vec(), true)
}

/// The same, for a caller that already owns the only copy.
///
/// A download hands back a `Vec` and then hands it to the cache, which used
/// to copy it — so for the moment before the caller's own dropped, the heap
/// held the attachment twice. On a desktop that is a copy into a file and
/// unavoidable; here both are the same linear memory, with a ceiling, and a
/// large document can be a meaningful fraction of it.
pub fn put_owned(key: &str, bytes: Vec<u8>) -> Result<String> {
    store(key, bytes, true)
}

/// Cache `bytes` under `key`, droppable from the moment they land.
///
/// For the eager copy of an inbound message's media, which nobody is waiting
/// on and which can be fetched again on demand. See [`super::put_since`],
/// whose whole subject is that this write is the one allowed to lose.
pub fn put_evictable(key: &str, bytes: &[u8]) -> Result<String> {
    store(key, bytes.to_vec(), false)
}

fn store(key: &str, bytes: Vec<u8>, pinned: bool) -> Result<String> {
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
                bytes: Arc::new(bytes),
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
        // Moved out where this was the only handle, which is the ordinary
        // case for the two callers: a staged upload nobody else read, and a
        // download answering one request. A clone only where some reader is
        // still holding the same payload, which is the case that was going to
        // cost a copy anyway.
        Some(Arc::try_unwrap(entry.bytes).unwrap_or_else(|shared| (*shared).clone()))
    })
}

/// Read what is under `key` and leave it there.
///
/// The desktop has no such call: its front end opens the file itself. Here
/// the front end is this process, so this is how a frame's media reaches it.
pub fn deliver(key: &str) -> Option<Arc<Vec<u8>>> {
    with(|cache| {
        let entry = cache.entries.get_mut(key)?;
        cache.clock += 1;
        entry.touched = cache.clock;
        // The delivery this entry was being held for has happened, so it goes
        // back to being an ordinary cache entry. Kept rather than removed:
        // another request for the same content is answered with the same key,
        // and a save that lost the browser's activation is about to ask again.
        entry.pinned = false;
        Some(Arc::clone(&entry.bytes))
    })
}

/// Read what is under `key` and leave it there, claim and all.
pub fn read(key: &str) -> Option<Arc<Vec<u8>>> {
    with(|cache| {
        let entry = cache.entries.get_mut(key)?;
        cache.clock += 1;
        entry.touched = cache.clock;
        Some(Arc::clone(&entry.bytes))
    })
}

/// Whether `key` is already cached, without reading it.
pub fn has(key: &str) -> bool {
    with(|cache| cache.entries.contains_key(key))
}

/// Claim `key` for a delivery, if it is here.
///
/// [`has`] asks; this one *takes responsibility*. The difference matters on
/// the one path that answers a request out of an entry it did not write: a
/// download whose bytes were already cached is reported successful
/// immediately, and between that answer and the front end reading it, another
/// media write's sweep — or a "clear cached media" — can take an entry
/// nothing is holding. The asker is then told a download it was promised is
/// missing.
///
/// So the promise and the claim are the same act. Touched as well as pinned,
/// because an entry about to be handed over is the last one the sweep should
/// be eyeing.
pub fn claim(key: &str) -> bool {
    with(|cache| {
        let Some(entry) = cache.entries.get_mut(key) else {
            return false;
        };
        cache.clock += 1;
        entry.touched = cache.clock;
        entry.pinned = true;
        true
    })
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
            // A pinned entry survives a *cache* clear, for the same reason it
            // survives the sweep: somebody asked for these bytes, the request
            // has already been answered `Ok`, and the reader is on its way.
            // "Clear cached media" means the copies of things that can be
            // fetched again; this one is a delivery in progress, and dropping
            // it turns a download WhatsApp completed into a failure.
            //
            // `Wipe::Everything` takes it regardless: there the account
            // itself is going, and nothing that was going to be shown to it
            // has any business outliving it.
            let taken = scope.takes(name) && !(entry.pinned && scope == Wipe::Cache);
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
///
/// A pinned entry is different, and the difference is the whole shape of
/// this: it is preferred, not untouchable. A pin says a delivery is on its
/// way, and a delivery can simply never arrive — the client that asked
/// disconnected, or timed out, and `answer_now` has nobody to hand the frame
/// to. Nothing releases the pin then. Treating pins as absolute would let a
/// run of abandoned downloads grow this map without limit, which is the one
/// thing a budget exists to prevent, so the budget wins in the end: unpinned
/// entries go first, and only if that is not enough do pinned ones follow,
/// oldest first.
///
/// Which means a pending delivery can still be dropped — under exactly the
/// pressure where dropping it beats exhausting the heap the page is drawn
/// with.
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

    // Sorted so that everything unpinned comes before anything pinned, and
    // each group runs oldest first. One list rather than two passes, because
    // "take the oldest until the budget is met" is one rule with a
    // tie-breaker rather than two policies.
    let mut oldest: Vec<(String, bool, u64, u64)> = cache
        .entries
        .iter()
        .filter(|(name, _)| !is_staged_upload(name))
        .map(|(name, entry)| {
            (
                name.clone(),
                entry.pinned,
                entry.touched,
                entry.bytes.len() as u64,
            )
        })
        .collect();
    oldest.sort_by_key(|(_, pinned, touched, _)| (*pinned, *touched));

    let mut over = reclaimable - CACHE_BUDGET_BYTES;
    for (name, pinned, _, size) in oldest {
        if over == 0 {
            break;
        }
        if pinned {
            log::warn!(
                "dropping {name}, which was waiting to be delivered: the media cache is over \
                 its budget with nothing unclaimed left to reclaim"
            );
        }
        cache.entries.remove(&name);
        cache.held = cache.held.saturating_sub(size);
        over = over.saturating_sub(size);
    }
}
