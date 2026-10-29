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

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use super::{Wipe, is_disposable};

/// How much media the page may hold before the oldest is dropped.
///
/// [`WEB_MEDIA_BUDGET_BYTES`] is where the number and the reasoning live: the
/// page's three media budgets are three ceilings on one heap, so they are one
/// number or they are their sum.
use oxidezap_core::WEB_MEDIA_BUDGET_BYTES as CACHE_BUDGET_BYTES;

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
    /// How many answered requests are still waiting to be handed these bytes.
    ///
    /// A count and not a flag, because the key is content-addressed: the same
    /// photo asked for from two different messages is one entry answering two
    /// `Downloaded` frames. A boolean cleared by the first delivery would
    /// leave the second one's bytes reclaimable while its frame was still in
    /// the pipe, which is the failure the claim exists to prevent, arrived at
    /// from the other side.
    ///
    /// A claim is the delivery of a request already answered `Ok`, so it is
    /// not a copy of something fetchable again *in time to matter* — the
    /// sweep leaves claimed entries for last. It does not make them
    /// untouchable: see [`sweep`].
    claims: usize,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    clock: u64,
    budget: Budget,
}

/// Count only entries the sweep may reclaim. Updated with each mutation so
/// inserting below the limits does not scan the whole cache.
#[derive(Default)]
struct Budget {
    bytes: u64,
    entries: usize,
}

impl Budget {
    fn insert(&mut self, key: &str, size: u64) {
        if is_disposable(key) {
            self.bytes += size;
            self.entries += 1;
        }
    }

    fn remove(&mut self, key: &str, size: u64) {
        if is_disposable(key) {
            self.bytes -= size;
            self.entries -= 1;
        }
    }
}

thread_local! {
    /// Thread-local rather than static, for the reason the store and the
    /// claim are: one agent owns this page's session, so a second one
    /// reaching for the cache is not a race to make unlikely but one to make
    /// impossible. It also keeps a `std::sync::Mutex` off a path the window
    /// thread takes — this module is built with `+atomics` and a shared
    /// memory, where a contended lock emits `memory.atomic.wait32`, which the
    /// main thread of a page may not execute at all.
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

fn with<T>(f: impl FnOnce(&mut Cache) -> T) -> T {
    CACHE.with(|cache| f(&mut cache.borrow_mut()))
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
    store(key, bytes.to_vec(), 1)
}

/// The same, for a caller that already owns the only copy.
///
/// A download hands back a `Vec` and then hands it to the cache, which used
/// to copy it — so for the moment before the caller's own dropped, the heap
/// held the attachment twice. On a desktop that is a copy into a file and
/// unavoidable; here both are the same linear memory, with a ceiling, and a
/// large document can be a meaningful fraction of it.
pub fn put_owned(key: &str, bytes: Vec<u8>) -> Result<String> {
    store(key, bytes, 1)
}

pub fn put_owned_evictable(key: &str, bytes: Vec<u8>) -> Result<String> {
    store(key, bytes, 0)
}

/// Cache `bytes` under `key`, droppable from the moment they land.
///
/// For the eager copy of an inbound message's media, which nobody is waiting
/// on and which can be fetched again on demand. See [`super::put_since`],
/// whose whole subject is that this write is the one allowed to lose.
pub fn put_evictable(key: &str, bytes: &[u8]) -> Result<String> {
    store(key, bytes.to_vec(), 0)
}

fn store(key: &str, bytes: Vec<u8>, claims: usize) -> Result<String> {
    with(|cache| {
        // Content-addressed: the same key is the same bytes, so an entry that
        // is already there is already right — and re-storing it would be a
        // second copy of a photo for as long as it took to replace the first.
        if let Some(entry) = cache.entries.get_mut(key) {
            cache.clock += 1;
            entry.touched = cache.clock;
            // Added, not replaced: a key first cached eagerly and then asked
            // for gains a claim, and a second asker gains another. An eager
            // write adds none, so it can never release bytes a reader is
            // coming for.
            entry.claims += claims;
            return Ok(key.to_string());
        }
        cache.clock += 1;
        let touched = cache.clock;
        cache.budget.insert(key, bytes.len() as u64);
        cache.entries.insert(
            key.to_string(),
            Entry {
                bytes: Arc::new(bytes),
                touched,
                claims,
            },
        );
        sweep(cache, key);
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
        cache.budget.remove(key, entry.bytes.len() as u64);
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
        // One delivery has happened. Only one: another request for the same
        // content is answered with the same key and is still waiting for it.
        // Kept rather than removed even at zero, because a save that lost the
        // browser's activation is about to ask again.
        entry.claims = entry.claims.saturating_sub(1);
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
/// So the promise and the claim are the same act. Touched as well as counted,
/// because an entry about to be handed over is the last one the sweep should
/// be eyeing.
pub fn claim(key: &str) -> bool {
    with(|cache| {
        let Some(entry) = cache.entries.get_mut(key) else {
            return false;
        };
        cache.clock += 1;
        entry.touched = cache.clock;
        entry.claims += 1;
        true
    })
}

/// What the media cache occupies: bytes, and how many entries.
pub fn cache_usage() -> (u64, u64) {
    usage_for("")
}

/// What one account's slice of the shared map occupies.
///
/// The named half of [`cache_usage`]: a storage pane for one account must not
/// bill it for every other account's entries in the same page.
pub fn usage_for(prefix: &str) -> (u64, u64) {
    with(|cache| {
        cache
            .entries
            .iter()
            .filter(|(name, _)| prefix.is_empty() || name.starts_with(prefix))
            .fold((0u64, 0u64), |(bytes, files), (_, entry)| {
                (bytes + entry.bytes.len() as u64, files + 1)
            })
    })
}

/// Delete the entries belonging to no account: the pre-multi-account names.
///
/// The lock and the epoch belong to [`super::wipe_for`], which is the only
/// caller.
pub(super) fn delete_unscoped(scope: Wipe) -> Result<()> {
    with(|cache| {
        let mut removed = 0u64;
        cache.entries.retain(|name, entry| {
            if oxidezap_ipc::account_prefix_of(name).is_some() {
                return true;
            }
            // A bare staged key is the daemon's now, not this account's: plugin
            // installation is global and reads `u-<name>` from whichever front
            // end asked, so a legacy-account wipe must not take a payload
            // another account's install is still going to consume.
            if oxidezap_ipc::is_global_staged_key(name) {
                return true;
            }
            let taken = scope.takes(name) && !(entry.claims > 0 && scope == Wipe::Cache);
            if taken {
                cache.budget.remove(name, entry.bytes.len() as u64);
                removed += 1;
            }
            !taken
        });
        log::info!("cleared {removed} pre-multi-account media entries ({scope:?})");
    });
    Ok(())
}

/// Delete only entries in one account namespace when `prefix` is non-empty.
pub(super) fn delete_for(prefix: &str, scope: Wipe) -> Result<()> {
    with(|cache| {
        cache.entries.retain(|name, entry| {
            let Some(local_name) = name.strip_prefix(prefix) else {
                return true;
            };
            // A claimed entry survives a *cache* clear, for the same reason it
            // survives the sweep: somebody asked for these bytes, the request
            // has already been answered `Ok`, and the reader is on its way.
            // "Clear cached media" means the copies of things that can be
            // fetched again; this one is a delivery in progress, and dropping
            // it turns a download WhatsApp completed into a failure.
            //
            // `Wipe::Everything` takes it regardless: there the account
            // itself is going, and nothing that was going to be shown to it
            // has any business outliving it.
            let taken = scope.takes(local_name) && !(entry.claims > 0 && scope == Wipe::Cache);
            if taken {
                cache.budget.remove(name, entry.bytes.len() as u64);
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
/// A claimed entry is different, and the difference is the whole shape of
/// this: it is preferred, not untouchable. A claim says a delivery is on its
/// way, and a delivery can simply never arrive — the client that asked
/// disconnected, or timed out, and `answer_now` has nobody to hand the frame
/// to. Nothing releases the claim then. Treating claims as absolute would let
/// a run of abandoned downloads grow this map without limit, which is the one
/// thing a budget exists to prevent — so the budget wins in the end: unclaimed
/// entries go first, and only if that is not enough do claimed ones follow,
/// oldest first.
///
/// Which means a pending delivery can still be dropped — under exactly the
/// pressure where dropping it beats exhausting the heap the page is drawn
/// with.
///
/// With one exception, and it is the entry `arriving` names: whatever has
/// just been written may not be reclaimed by its own insertion. A payload
/// larger than the whole budget is over it the moment it lands, and every
/// other entry taken together cannot bring it back under — so the sweep
/// reached the new entry, dropped it, and `store` returned its key anyway.
/// What the person saw was a download WhatsApp completed, reported
/// successful, and then missing: deterministic for any attachment over the
/// budget, which is the size at which having the bytes matters most.
///
/// Sparing it bounds nothing worse than one payload: it is counted, so
/// everything else reclaimable is still taken to make room for it, and the
/// *next* write's sweep sees it as an ordinary claimed entry and may drop it
/// then. The map therefore holds at most the budget plus whatever single
/// thing arrived last.
const MAX_CACHE_ENTRIES: usize = 4096;

fn sweep(cache: &mut Cache, arriving: &str) {
    if cache.budget.bytes <= CACHE_BUDGET_BYTES && cache.budget.entries <= MAX_CACHE_ENTRIES {
        return;
    }

    // Sorted so that everything unclaimed comes before anything claimed, and
    // each group runs oldest first. One list rather than two passes, because
    // "take the oldest until the budget is met" is one rule with a
    // tie-breaker rather than two policies.
    let mut oldest: Vec<(String, bool, u64, u64)> = cache
        .entries
        .iter()
        .filter(|(name, _)| is_disposable(name) && name.as_str() != arriving)
        .map(|(name, entry)| {
            (
                name.clone(),
                entry.claims > 0,
                entry.touched,
                entry.bytes.len() as u64,
            )
        })
        .collect();
    oldest.sort_by_key(|(_, claimed, touched, _)| (*claimed, *touched));

    for (name, claimed, _, size) in oldest {
        if cache.budget.bytes <= CACHE_BUDGET_BYTES && cache.budget.entries <= MAX_CACHE_ENTRIES {
            break;
        }
        if claimed {
            log::warn!(
                "dropping {name}, which was waiting to be delivered: the media cache is over \
                 its budget with nothing unclaimed left to reclaim"
            );
        }
        cache.entries.remove(&name);
        cache.budget.remove(&name, size);
    }
}

#[cfg(test)]
mod owned_tests {
    use super::*;

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn epoch_checked_download_transfers_allocation_without_a_claim() {
        let account =
            super::super::AccountMedia::new(oxidezap_core::AccountId::new(990003).unwrap());
        account.wipe(Wipe::Cache).unwrap();
        let epoch = account.epoch();
        let bytes = vec![7u8; 4096];
        let allocation = bytes.as_ptr();
        let key = account
            .put_owned_since(epoch, "d-owned-fixture", bytes)
            .unwrap();
        with(|cache| {
            let entry = cache.entries.get(&key).unwrap();
            assert_eq!(entry.bytes.as_ptr(), allocation);
            assert_eq!(entry.claims, 0);
        });
        account.wipe(Wipe::Cache).unwrap();
        assert!(
            account
                .put_owned_since(epoch, "d-stale-fixture", vec![1])
                .is_err()
        );
        assert!(!has(&account.key("d-stale-fixture")));
    }

    fn assert_budget_matches_entries() {
        with(|cache| {
            let mut bytes = 0;
            let mut entries = 0;
            for (key, entry) in &cache.entries {
                if is_disposable(key) {
                    bytes += entry.bytes.len() as u64;
                    entries += 1;
                }
            }
            assert_eq!(cache.budget.bytes, bytes);
            assert_eq!(cache.budget.entries, entries);
        });
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn budget_tracks_duplicates_takes_and_account_wipes() {
        let prefix = "a990004-";
        delete_for(prefix, Wipe::Everything).unwrap();
        for suffix in [
            "f-photo",
            "d-download",
            "a-avatar",
            "m-legacy",
            "u-send",
            "other",
        ] {
            let key = format!("{prefix}{suffix}");
            put_owned_evictable(&key, vec![7; 32]).unwrap();
            assert_budget_matches_entries();
            // The existing payload and its budget stay unchanged, while a
            // requested write gains a claim even when its bytes differ.
            put_owned(&key, vec![9; 64]).unwrap();
            assert_eq!(read(&key).unwrap().len(), 32);
            assert_budget_matches_entries();
            deliver(&key).unwrap();
        }
        let claimed = format!("{prefix}d-claimed");
        put_owned(&claimed, vec![1; 12]).unwrap();
        delete_for(prefix, Wipe::Cache).unwrap();
        assert!(has(&claimed));
        assert!(has(&format!("{prefix}u-send")));
        assert!(!has(&format!("{prefix}f-photo")));
        assert_budget_matches_entries();
        take(&format!("{prefix}u-send")).unwrap();
        take(&claimed).unwrap();
        assert_budget_matches_entries();
        delete_for(prefix, Wipe::Everything).unwrap();
        assert_budget_matches_entries();
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn budget_tracks_entry_limit_evictions_and_spares_staging() {
        let prefix = "a990005-";
        delete_for(prefix, Wipe::Everything).unwrap();
        let staged = format!("{prefix}u-send");
        put_owned_evictable(&staged, vec![1; 8]).unwrap();
        for index in 0..MAX_CACHE_ENTRIES + 3 {
            put_owned_evictable(&format!("{prefix}d-{index}"), Vec::new()).unwrap();
        }
        assert!(has(&staged));
        assert!(!has(&format!("{prefix}d-0")));
        assert!(has(&format!("{prefix}d-{}", MAX_CACHE_ENTRIES + 2)));
        assert_budget_matches_entries();
        delete_for(prefix, Wipe::Everything).unwrap();
        assert_budget_matches_entries();
    }

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn budget_tracks_unscoped_removal_and_oversized_arrivals() {
        let key = "d-budget-unscoped-fixture";
        put_owned_evictable(key, vec![1; 8]).unwrap();
        delete_unscoped(Wipe::Cache).unwrap();
        assert!(!has(key));
        assert_budget_matches_entries();

        let prefix = "a990006-";
        let large = format!("{prefix}d-large");
        put_owned(&large, vec![1; CACHE_BUDGET_BYTES as usize + 1]).unwrap();
        assert!(has(&large), "an arrival cannot evict itself");
        assert_budget_matches_entries();
        put_owned_evictable(&format!("{prefix}d-next"), vec![2]).unwrap();
        assert!(!has(&large), "a later arrival may evict a claimed entry");
        assert_budget_matches_entries();
        delete_for(prefix, Wipe::Everything).unwrap();
        assert_budget_matches_entries();
    }
}
