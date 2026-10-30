//! Avatar loading, demand management, and multi-tier caching.
//!
//! Lifecycle:
//! 1. Memory Tier (RAM): Decoded [`gpui::ImageSource`] in `AVATAR_IMAGES` LRU cache.
//! 2. Persistent Tier:
//!    - Web: Browser Cache Storage API (`window.caches`). Survives F5 reloads.
//!    - Native: Daemon media directory (`oxidezap_ipc::media_path`).
//! 3. Network Tier: Demand-driven metadata and profile-picture lookups via
//!    session `EnsureAvatars`.

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(target_family = "wasm")]
mod web;
#[cfg(target_family = "wasm")]
mod web_lock;
#[cfg(target_family = "wasm")]
mod web_platform;

#[cfg(all(test, not(target_family = "wasm")))]
mod probe;

#[cfg(not(target_family = "wasm"))]
use native as imp;
#[cfg(target_family = "wasm")]
use web_platform as imp;

use portable_atomic::{AtomicU64, Ordering};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use oxidezap_core::{AccountId, AvatarDemand};
use wacore::time::Instant;

use crate::session::media::{clear_image_sources, get_avatar_image, put_avatar_image};

#[allow(unused_imports)]
pub use imp::{
    active_account, avatar_cache_usage, clear_cache_storage, delete_account_storage,
    delete_persistent, purge_legacy_caches, read_persistent, rotate_account_scope,
    set_active_account, spawn_task, write_persistent,
};

/// Persist an avatar payload to the platform's storage tier.
#[allow(dead_code)]
pub fn save_avatar(key: &str, bytes: &[u8]) {
    #[cfg(target_family = "wasm")]
    {
        let key = key.to_string();
        let bytes = bytes.to_vec();
        let account = imp::resolve_account(&key);
        let Ok(stamp) = imp::persistent_stamp(account) else {
            return;
        };
        spawn_task(async move {
            let _ = write_persistent(&key, &bytes, account, stamp).await;
        });
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let _ = (key, bytes);
    }
}

/// One avatar demand description passed by the viewport or header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatDemand {
    pub jid: String,
    pub picture_id: Option<String>,
    pub cache_key: Option<String>,
}

/// Priority of an avatar demand: visible rows and active conversation header
/// take high priority; overscan rows take low priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemandPriority {
    High,
    Low,
}

type DecodePriority = Arc<portable_atomic::AtomicBool>;

fn decode_priority(priority: DemandPriority) -> DecodePriority {
    Arc::new(portable_atomic::AtomicBool::new(
        priority == DemandPriority::High,
    ))
}

struct PendingRead {
    token: u64,
    priority: DecodePriority,
}

impl PendingRead {
    fn new(token: u64, priority: DemandPriority) -> Self {
        Self {
            token,
            priority: decode_priority(priority),
        }
    }
}

const MAX_TRACKED_AVATARS: usize = 2048;
const MAX_PENDING_DEMANDS: usize = 256;
const MAX_PERSISTENT_READS: usize = 64;

static NEXT_OP_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Central avatar manager coordinating viewport demands, persistent storage,
/// and in-memory decoded image caching.
#[derive(Clone, Default)]
pub struct AvatarManager {
    persistent_in_flight: Arc<Mutex<HashMap<(String, String), PendingRead>>>,
    network_in_flight: Arc<Mutex<HashSet<String>>>,
    revalidated: Arc<Mutex<indexmap::IndexSet<String>>>,
    failed_cooldowns: Arc<Mutex<HashMap<String, (bool, Instant)>>>,
    pending_demands: Arc<Mutex<HashMap<String, (AvatarDemand, DemandPriority)>>>,
    deferred_misses: Arc<Mutex<indexmap::IndexMap<String, String>>>,
    generation: Arc<AtomicU64>,
    demand_epoch: Arc<AtomicU64>,
    account_id: Arc<Mutex<Option<AccountId>>>,
    session: Arc<Mutex<Option<crate::session::SessionHandle>>>,
}

impl AvatarManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn demand_epoch(&self) -> u64 {
        // The viewport fingerprint must stop matching when decoded images
        // disappear, even when the rows and their durable keys stay the same.
        self.demand_epoch
            .load(Ordering::Relaxed)
            .wrapping_add(crate::session::media::avatar_cache_epoch())
    }

    pub fn bump_demand_epoch(&self) -> u64 {
        self.demand_epoch.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Set or clear the active session handle, syncing active account and generation.
    pub fn set_session(&self, session: Option<crate::session::SessionHandle>) {
        let new_account = session.as_ref().map(|s| s.account());
        *self.account_id.lock().unwrap_or_else(|e| e.into_inner()) = new_account;
        set_active_account(new_account);
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.bump_demand_epoch();
        self.reset_connection();
        let mut s = self.session.lock().unwrap_or_else(|e| e.into_inner());
        *s = session;
        spawn_task(async {
            purge_legacy_caches().await;
        });
    }

    /// Reset in-memory cache and tracking (e.g. on cache clear or account change).
    pub fn clear(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.bump_demand_epoch();
        self.reset_connection();
        // Retire reads before clearing images. A worker publishing while
        // reset_connection waits for its read lock is then cleared as well.
        clear_image_sources();
    }

    /// Clear in-flight, revalidated, and cooldown tracking on connection restart while
    /// preserving decoded images in memory.
    pub fn reset_connection(&self) {
        self.deferred_misses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.persistent_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.network_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.revalidated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.failed_cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.pending_demands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Queue a demand for one chat (e.g. from the chat header or active conversation)
    /// with high priority.
    pub fn demand_chat(&self, chat: &ChatDemand) {
        self.demand_one(chat, DemandPriority::High);
    }

    /// Process a batch of demands with high priority.
    #[allow(dead_code)]
    pub fn demand_chats(&self, chats: &[ChatDemand]) {
        for chat in chats {
            self.demand_one(chat, DemandPriority::High);
        }
    }

    /// Process viewport demands with visible rows (high priority) and overscan rows
    /// (low priority), pruning stale overscan demands on fast scroll.
    pub fn demand_chats_windowed(&self, visible: &[ChatDemand], overscan: &[ChatDemand]) {
        for chat in visible {
            self.demand_one(chat, DemandPriority::High);
        }
        // A batch of local reads is scheduled asynchronously, so spawning
        // overscan last does not ensure it runs last. Let visible disk hits
        // reach RAM before admitting speculative local work. Completion
        // advances demand_epoch and wakes the viewport to admit overscan.
        let visible_loading = {
            let reads = self
                .persistent_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            visible
                .iter()
                .any(|chat| reads.keys().any(|(jid, _)| jid == &chat.jid))
        };
        if !visible_loading {
            for chat in overscan {
                self.demand_one(chat, DemandPriority::Low);
            }
        }

        // Prune low-priority pending demands that are no longer in the active window
        let mut active_jids = HashSet::with_capacity(visible.len() + overscan.len());
        for c in visible.iter().chain(overscan.iter()) {
            active_jids.insert(&c.jid);
        }
        let mut pending = self
            .pending_demands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut cancelled = Vec::new();
        pending.retain(|jid, (_demand, priority)| {
            let keep = *priority == DemandPriority::High || active_jids.contains(jid);
            if !keep {
                cancelled.push(jid.clone());
            }
            keep
        });
        drop(pending);
        // These requests never left the queue. Release their reservations so
        // scrolling back can ask again, including freshness-only demands.
        let mut network = self
            .network_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut revalidated = self.revalidated.lock().unwrap_or_else(|e| e.into_inner());
        for jid in cancelled {
            network.remove(&jid);
            revalidated.shift_remove(&jid);
        }
    }

    fn demand_one(&self, chat: &ChatDemand, priority: DemandPriority) {
        let jid = &chat.jid;

        // Check failure cooldown: avoid spinning on recently failed avatars
        {
            let mut cooldowns = self
                .failed_cooldowns
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some((_retryable, until)) = cooldowns.get(jid) {
                if Instant::now() < *until {
                    return;
                }
                cooldowns.remove(jid);
            }
        }

        // If a network fetch is already active for this JID, avoid redundant checks
        if self
            .network_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(jid)
        {
            return;
        }

        if let Some(key) = &chat.cache_key {
            if get_avatar_image(key).is_some() {
                // Cache hit in RAM: check if revalidation is needed for this connection
                let mut reval = self.revalidated.lock().unwrap_or_else(|e| e.into_inner());
                if note_revalidation(&mut reval, jid) {
                    let queued = self.enqueue_demand(
                        AvatarDemand {
                            jid: jid.clone(),
                            known_picture_id: chat.picture_id.clone(),
                            cache_key: Some(key.clone()),
                            need_bytes: false,
                            cache_miss: false,
                        },
                        priority,
                    );
                    if !queued {
                        reval.shift_remove(jid);
                    }
                }
                return;
            }

            // A previous read already established that this exact blob is
            // absent. Retry network admission without reading it again on
            // every repaint while the network queue remains full.
            let deferred = self
                .deferred_misses
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(jid)
                == Some(key);
            if deferred {
                let reserved = {
                    let mut network = self
                        .network_in_flight
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    network.len() < MAX_PENDING_DEMANDS && network.insert(jid.clone())
                };
                if reserved {
                    if self.enqueue_demand(
                        AvatarDemand {
                            jid: jid.clone(),
                            known_picture_id: None,
                            cache_key: None,
                            need_bytes: true,
                            cache_miss: true,
                        },
                        priority,
                    ) {
                        self.deferred_misses
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .shift_remove(jid);
                    } else {
                        self.network_in_flight
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(jid);
                    }
                }
                return;
            }

            // Miss in RAM: check persistent store if not already in flight
            let op_token = NEXT_OP_TOKEN.fetch_add(1, Ordering::Relaxed);
            let mut p_flight = self
                .persistent_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(read) = p_flight.get(&(jid.clone(), key.clone())) {
                if priority == DemandPriority::High {
                    read.priority.store(true, Ordering::Relaxed);
                }
                return;
            }
            if p_flight.len() >= MAX_PERSISTENT_READS {
                return;
            }
            if let std::collections::hash_map::Entry::Vacant(entry) =
                p_flight.entry((jid.clone(), key.clone()))
            {
                let reservation = PendingRead::new(op_token, priority);
                let decode_priority = reservation.priority.clone();
                entry.insert(reservation);
                let key_clone = key.clone();
                let jid_clone = jid.clone();
                let pic_id = chat.picture_id.clone();
                let read_key = (jid.clone(), key.clone());
                let persistent_in_flight = Arc::clone(&self.persistent_in_flight);
                let network_in_flight = Arc::clone(&self.network_in_flight);
                let revalidated = Arc::clone(&self.revalidated);
                let pending_demands = Arc::clone(&self.pending_demands);
                let session_handle = Arc::clone(&self.session);
                let generation = Arc::clone(&self.generation);
                let demand_epoch = Arc::clone(&self.demand_epoch);
                let account_id = Arc::clone(&self.account_id);
                let deferred_misses = Arc::clone(&self.deferred_misses);
                let born_gen = generation.load(Ordering::Relaxed);
                let born_account = *account_id.lock().unwrap_or_else(|e| e.into_inner());

                spawn_task(async move {
                    let persistent_bytes = read_persistent(&key_clone).await;
                    demand_epoch.fetch_add(1, Ordering::Relaxed);
                    // A network selection/removal may have retired this read.
                    if persistent_in_flight
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&read_key)
                        .map(|read| read.token)
                        != Some(op_token)
                    {
                        return;
                    }

                    // Generation safety: discard if session/account changed while reading
                    if generation.load(Ordering::Relaxed) != born_gen
                        || *account_id.lock().unwrap_or_else(|e| e.into_inner()) != born_account
                    {
                        let mut p = persistent_in_flight
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if p.get(&read_key).map(|read| read.token) == Some(op_token) {
                            p.remove(&read_key);
                        }
                        return;
                    }

                    if let Some(bytes) = persistent_bytes {
                        let current = {
                            let pending = persistent_in_flight.clone();
                            let generation = generation.clone();
                            let account = account_id.clone();
                            let key = read_key.clone();
                            move || {
                                let pending = pending.lock().unwrap_or_else(|e| e.into_inner());
                                pending.get(&key).map(|read| read.token) == Some(op_token)
                                    && generation.load(Ordering::Relaxed) == born_gen
                                    && *account.lock().unwrap_or_else(|e| e.into_inner())
                                        == born_account
                            }
                        };
                        let decoded = imp::decode_avatar(bytes, decode_priority, current).await;
                        {
                            let mut p = persistent_in_flight
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            // A decode can outlive replacement, cache clear or
                            // account change. Check again after the worker's
                            // await, holding the read reservation through RAM
                            // publication so retirement cannot overtake it.
                            if p.get(&read_key).map(|read| read.token) != Some(op_token)
                                || generation.load(Ordering::Relaxed) != born_gen
                                || *account_id.lock().unwrap_or_else(|e| e.into_inner())
                                    != born_account
                            {
                                if p.get(&read_key).map(|read| read.token) == Some(op_token) {
                                    p.remove(&read_key);
                                }
                                return;
                            }
                            if let Some((source, decoded_size)) = &decoded {
                                put_avatar_image(key_clone.clone(), source.clone(), *decoded_size);
                            }
                        }
                        if decoded.is_some() {
                            let mut p = persistent_in_flight
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if p.get(&read_key).map(|read| read.token) != Some(op_token) {
                                return;
                            }
                            p.remove(&read_key);
                            drop(p);
                            // Disk reads can all finish while decodes are
                            // queued. Releasing a decode also releases local
                            // admission, so a viewport refused at the read
                            // limit must get another fingerprint and retry.
                            demand_epoch.fetch_add(1, Ordering::Relaxed);

                            // Notify UI that the avatar image is ready in RAM
                            if let Some(session) = session_handle
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .as_ref()
                            {
                                session.notify_avatar_loaded();
                            }

                            // Conditional freshness revalidation:
                            let mut reval = revalidated.lock().unwrap_or_else(|e| e.into_inner());
                            if note_revalidation(&mut reval, &jid_clone) {
                                let demand = AvatarDemand {
                                    jid: jid_clone,
                                    known_picture_id: pic_id,
                                    cache_key: Some(key_clone),
                                    need_bytes: false,
                                    cache_miss: false,
                                };
                                if let Some(session) = session_handle
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .as_ref()
                                {
                                    session.ensure_avatars(vec![demand]);
                                } else {
                                    pending_demands
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .insert(demand.jid.clone(), (demand, priority));
                                }
                            }
                            return;
                        }

                        // Corrupt persistent blob: delete entry
                        let _ = delete_persistent(&key_clone).await;

                        // Re-validate generation/account post-await
                        if generation.load(Ordering::Relaxed) != born_gen
                            || *account_id.lock().unwrap_or_else(|e| e.into_inner()) != born_account
                        {
                            let mut p = persistent_in_flight
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if p.get(&read_key).map(|read| read.token) == Some(op_token) {
                                p.remove(&read_key);
                            }
                            return;
                        }
                    }

                    // Persistent miss or corrupt blob: unconditionally fetch bytes
                    {
                        let mut p = persistent_in_flight
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if p.get(&read_key).map(|read| read.token) != Some(op_token) {
                            return;
                        }
                        p.remove(&read_key);
                    }
                    let reserved = {
                        let mut n_flight =
                            network_in_flight.lock().unwrap_or_else(|e| e.into_inner());
                        n_flight.len() < MAX_PENDING_DEMANDS && n_flight.insert(jid_clone.clone())
                    };
                    if !reserved {
                        let mut deferred =
                            deferred_misses.lock().unwrap_or_else(|e| e.into_inner());
                        if deferred.len() >= MAX_TRACKED_AVATARS
                            && !deferred.contains_key(&jid_clone)
                        {
                            deferred.shift_remove_index(0);
                        }
                        deferred.insert(jid_clone.clone(), key_clone.clone());
                    }
                    // A miss also releases visible local admission. Wake the
                    // viewport even offline, so deferred overscan can recover
                    // without waiting for a successful network response.
                    demand_epoch.fetch_add(1, Ordering::Relaxed);
                    if let Some(session) = session_handle
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                    {
                        session.notify_avatar_loaded();
                    }
                    if reserved {
                        let demand = AvatarDemand {
                            jid: jid_clone,
                            known_picture_id: None,
                            cache_key: None,
                            need_bytes: true,
                            cache_miss: true,
                        };
                        if let Some(session) = session_handle
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_ref()
                        {
                            session.ensure_avatars(vec![demand]);
                        } else {
                            pending_demands
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(demand.jid.clone(), (demand, priority));
                        }
                    }
                });
            }
        } else {
            if self
                .revalidated
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(jid)
            {
                return;
            }
            let reserved = {
                let mut in_flight = self
                    .network_in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if in_flight.len() >= MAX_PENDING_DEMANDS {
                    return;
                }
                in_flight.insert(jid.clone())
            };
            if reserved
                && !self.enqueue_demand(
                    AvatarDemand {
                        jid: jid.clone(),
                        known_picture_id: None,
                        cache_key: None,
                        need_bytes: true,
                        // The daemon may remember BytesReady after the
                        // derived descriptor was retired or never hydrated.
                        cache_miss: true,
                    },
                    priority,
                )
            {
                self.network_in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(jid);
            }
        }
    }

    pub fn enqueue_demand(&self, demand: AvatarDemand, priority: DemandPriority) -> bool {
        let mut pending = self
            .pending_demands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if pending.len() >= MAX_PENDING_DEMANDS && !pending.contains_key(&demand.jid) {
            return false;
        }
        pending
            .entry(demand.jid.clone())
            .and_modify(|(existing_demand, existing_priority)| {
                if demand.need_bytes {
                    existing_demand.need_bytes = true;
                    existing_demand.known_picture_id = None;
                }
                existing_demand.cache_miss |= demand.cache_miss;
                if priority == DemandPriority::High {
                    *existing_priority = DemandPriority::High;
                }
            })
            .or_insert((demand, priority));
        true
    }

    /// Extract all pending demands ordered by priority (high priority first).
    pub fn take_demands(&self) -> Vec<AvatarDemand> {
        let mut pending = self
            .pending_demands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut high = Vec::new();
        let mut low = Vec::new();
        for (_, (demand, priority)) in pending.drain() {
            match priority {
                DemandPriority::High => high.push(demand),
                DemandPriority::Low => low.push(demand),
            }
        }
        high.extend(low);
        high
    }

    /// Flush accumulated demands to the session in chunks of up to 64 items.
    pub fn flush_demands(&self, session: &crate::session::SessionHandle) {
        let demands = self.take_demands();
        if demands.is_empty() {
            return;
        }

        for chunk in demands.chunks(64) {
            session.ensure_avatars(chunk.to_vec());
        }
    }

    /// Called when the daemon signals an avatar is ready (or cleared).
    pub fn on_avatar_ready(&self, jid: &str, key: &str) {
        self.deferred_misses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shift_remove(jid);
        self.network_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(jid);
        self.persistent_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(pending_jid, _), _| pending_jid != jid);
        self.failed_cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(jid);
        let mut revalidated = self.revalidated.lock().unwrap_or_else(|e| e.into_inner());
        if key.is_empty() {
            note_revalidation(&mut revalidated, jid);
        } else {
            revalidated.shift_remove(jid);
        }
        drop(revalidated);
        self.wake_deferred_misses();
    }

    fn wake_deferred_misses(&self) {
        if self
            .deferred_misses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            return;
        }
        self.bump_demand_epoch();
        if let Some(session) = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            session.notify_avatar_loaded();
        }
    }

    /// Called when an avatar lookup, download, or materialization fails.
    pub fn on_avatar_failed(&self, jid: &str, retryable: bool) -> Instant {
        self.revalidated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shift_remove(jid);
        self.network_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(jid);
        let cooldown_duration = if retryable {
            std::time::Duration::from_secs(15)
        } else {
            std::time::Duration::from_secs(300)
        };
        let until = Instant::now() + cooldown_duration;
        let mut cooldowns = self
            .failed_cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cooldowns.len() >= MAX_TRACKED_AVATARS {
            cooldowns.retain(|_, (_, deadline)| *deadline > Instant::now());
            if cooldowns.len() >= MAX_TRACKED_AVATARS
                && !cooldowns.contains_key(jid)
                && let Some(oldest) = cooldowns
                    .iter()
                    .min_by_key(|(_, (_, deadline))| *deadline)
                    .map(|(key, _)| key.clone())
            {
                cooldowns.remove(&oldest);
            }
        }
        cooldowns.insert(jid.to_string(), (retryable, until));
        drop(cooldowns);
        self.wake_deferred_misses();
        until
    }

    #[cfg(test)]
    pub fn is_in_flight(&self, item: &str) -> bool {
        self.network_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(item)
            || self
                .persistent_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .keys()
                .any(|(jid, key)| jid == item || key == item)
    }

    #[cfg(test)]
    pub fn is_revalidated(&self, jid: &str) -> bool {
        self.revalidated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(jid)
    }
}

fn note_revalidation(revalidated: &mut indexmap::IndexSet<String>, jid: &str) -> bool {
    if revalidated.contains(jid) {
        return false;
    }
    if revalidated.len() >= MAX_TRACKED_AVATARS {
        revalidated.shift_remove_index(0);
    }
    revalidated.insert(jid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::media::decode_avatar;

    // Run the real native storage and manager together in a separate process:
    // changing a process-wide runtime path in a parallel test would risk the
    // user's cache, and clearing the shared RAM cache would race other tests.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn native_cache_recovers_after_remount_eviction_and_corruption() {
        const ROOT: &str = "OXIDEZAP_AVATAR_TEST_ROOT";
        let Some(root) = std::env::var_os(ROOT) else {
            let root = std::env::temp_dir()
                .join(format!("oxidezap-avatar-lifecycle-{}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "session::avatar::tests::native_cache_recovers_after_remount_eviction_and_corruption",
                    "--nocapture",
                ])
                .env(ROOT, &root)
                .env("XDG_RUNTIME_DIR", &root)
                .env("LOCALAPPDATA", &root)
                .env("OXIDEZAP_ACCOUNT", "avatar-fixture")
                .status()
                .unwrap();
            let _ = std::fs::remove_dir_all(root);
            assert!(status.success(), "isolated avatar lifecycle failed");
            return;
        };
        let root = std::path::PathBuf::from(root);
        let key = "avatar-lifecycle-fixture";
        let path = oxidezap_ipc::media_path(key).unwrap();
        assert!(
            path.starts_with(&root),
            "fixture must not use the user's cache"
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&path, bytes.into_inner()).unwrap();
        let chat = ChatDemand {
            jid: "avatar@example.invalid".to_string(),
            picture_id: Some("picture-1".to_string()),
            cache_key: Some(key.to_string()),
        };
        smol::block_on(async {
            async fn next_batch(manager: &AvatarManager, count: usize) -> Vec<AvatarDemand> {
                let deadline = Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let ready = manager.pending_demands.lock().unwrap().len() >= count;
                    if ready {
                        return manager.take_demands();
                    }
                    assert!(Instant::now() < deadline, "avatar demand did not complete");
                    crate::platform::sleep(std::time::Duration::from_millis(1)).await;
                }
            }
            async fn next(manager: &AvatarManager) -> AvatarDemand {
                let mut demands = next_batch(manager, 1).await;
                assert_eq!(demands.len(), 1);
                demands.remove(0)
            }
            let first = AvatarManager::new();
            first.demand_chat(&chat);
            let demand = next(&first).await;
            assert!(!demand.need_bytes, "disk hit needs only freshness metadata");
            assert_eq!(demand.known_picture_id, chat.picture_id);

            // Offline failures in visible local recovery must release and
            // advance admission so valid overscan hits can still load.
            for corrupt in [false, true] {
                clear_image_sources();
                let missing = ChatDemand {
                    jid: "offline-miss@example.invalid".into(),
                    picture_id: Some("missing-picture".into()),
                    cache_key: Some("avatar-offline-miss".into()),
                };
                let missing_path =
                    oxidezap_ipc::media_path(missing.cache_key.as_ref().unwrap()).unwrap();
                if corrupt {
                    std::fs::write(&missing_path, b"corrupt fixture").unwrap();
                }
                let recovery = AvatarManager::new();
                let epoch = recovery.demand_epoch();
                recovery.demand_chats_windowed(
                    std::slice::from_ref(&missing),
                    std::slice::from_ref(&chat),
                );
                // Keep the visible miss queued while the viewport repeats.
                // Overscan can finish in the same batch if the visible read
                // completes before demand_chats_windowed checks admission.
                // Require both JIDs instead of assuming one completion per poll.
                let deadline = Instant::now() + std::time::Duration::from_secs(5);
                while !recovery
                    .pending_demands
                    .lock()
                    .unwrap()
                    .contains_key(&missing.jid)
                {
                    assert!(Instant::now() < deadline, "visible miss did not complete");
                    crate::platform::sleep(std::time::Duration::from_millis(1)).await;
                }
                assert_ne!(
                    recovery.demand_epoch(),
                    epoch,
                    "failed reads must invalidate the demand fingerprint"
                );
                recovery.demand_chats_windowed(
                    std::slice::from_ref(&missing),
                    std::slice::from_ref(&chat),
                );
                let demands = next_batch(&recovery, 2).await;
                assert_eq!(demands.len(), 2);
                assert_eq!(demands[0].jid, missing.jid, "visible demand has priority");
                assert!(demands[0].need_bytes && demands[0].cache_miss);
                assert_eq!(demands[1].jid, chat.jid);
                assert!(
                    !demands[1].need_bytes,
                    "offline overscan must recover from disk"
                );
                assert_eq!(demands[1].known_picture_id, chat.picture_id);
                assert!(get_avatar_image(key).is_some());
                assert!(!missing_path.exists());
            }

            // Saturated network admission must not turn its own completion
            // wake into repeated filesystem reads. Capacity release retries
            // the known miss directly, without another disk lookup.
            let saturated = AvatarManager::new();
            saturated
                .network_in_flight
                .lock()
                .unwrap()
                .extend((0..MAX_PENDING_DEMANDS).map(|i| format!("saturated-{i}@example.invalid")));
            let missing = ChatDemand {
                jid: "deferred-miss@example.invalid".into(),
                picture_id: Some("missing-picture".into()),
                cache_key: Some("avatar-deferred-miss".into()),
            };
            let reads = native::PERSISTENT_READS.load(std::sync::atomic::Ordering::Relaxed);
            saturated.demand_chat(&missing);
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            while !saturated
                .deferred_misses
                .lock()
                .unwrap()
                .contains_key(&missing.jid)
            {
                assert!(Instant::now() < deadline);
                crate::platform::sleep(std::time::Duration::from_millis(1)).await;
            }
            for _ in 0..10 {
                saturated.demand_chat(&missing);
            }
            assert!(saturated.take_demands().is_empty());
            assert_eq!(
                native::PERSISTENT_READS.load(std::sync::atomic::Ordering::Relaxed),
                reads + 1
            );
            let epoch = saturated.demand_epoch();
            saturated.on_avatar_failed("saturated-0@example.invalid", true);
            assert_ne!(
                saturated.demand_epoch(),
                epoch,
                "capacity release must wake deferred demand"
            );
            saturated.demand_chat(&missing);
            let retry = saturated.take_demands();
            assert_eq!(retry.len(), 1);
            assert!(retry[0].need_bytes && retry[0].cache_miss);
            assert_eq!(
                native::PERSISTENT_READS.load(std::sync::atomic::Ordering::Relaxed),
                reads + 1,
                "capacity retry must not read disk again"
            );

            let freshness = AvatarManager::new();
            freshness.demand_chats_windowed(&[], std::slice::from_ref(&chat));
            assert!(freshness.is_revalidated(&chat.jid));
            freshness.demand_chats_windowed(&[], &[]);
            assert!(!freshness.is_revalidated(&chat.jid));
            freshness.demand_chats_windowed(std::slice::from_ref(&chat), &[]);
            let demands = freshness.take_demands();
            assert_eq!(demands.len(), 1, "cancelled freshness must be retried");
            assert!(!demands[0].need_bytes);

            // Real LRU eviction changes the viewport fingerprint and restores
            // from disk without requesting the image bytes again.
            let epoch = first.demand_epoch();
            let (image, _) = decode_avatar(&std::fs::read(&path).unwrap()).unwrap();
            put_avatar_image(
                "avatar-fixture-pressure".to_string(),
                image,
                oxidezap_core::DECODED_IMAGE_BUDGET_BYTES,
            );
            assert!(get_avatar_image(key).is_none());
            assert_ne!(first.demand_epoch(), epoch);
            let remounted = AvatarManager::new();
            remounted.demand_chat(&chat);
            assert!(!next(&remounted).await.need_bytes);

            clear_image_sources();
            std::fs::remove_file(&path).unwrap();
            let reopened = AvatarManager::new();
            reopened.demand_chat(&chat);
            let missing = next(&reopened).await;
            assert!(missing.need_bytes && missing.cache_miss);
            assert!(missing.known_picture_id.is_none());

            std::fs::write(&path, b"corrupt avatar fixture").unwrap();
            let corrupt = AvatarManager::new();
            corrupt.demand_chat(&chat);
            let replacement = next(&corrupt).await;
            assert!(replacement.need_bytes && replacement.cache_miss);
            assert!(replacement.known_picture_id.is_none());
            assert!(
                !path.exists(),
                "corrupt bytes must be removed before refetch"
            );

            // Four valid 1024px pictures exceed the decoded budget unless
            // recovery retains thumbnails. They must settle in RAM together,
            // without repeatedly invalidating the same stationary viewport.
            let mut large = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgba8(1024, 1024)
                .write_to(&mut large, image::ImageFormat::Png)
                .unwrap();
            let large = large.into_inner();
            let window = AvatarManager::new();
            let mut chats = Vec::new();
            for index in 0..4 {
                let key = format!("avatar-large-fixture-{index}");
                let path = oxidezap_ipc::media_path(&key).unwrap();
                std::fs::write(&path, &large).unwrap();
                let chat = ChatDemand {
                    jid: format!("avatar-{index}@example.invalid"),
                    picture_id: Some("large-picture".to_string()),
                    cache_key: Some(key),
                };
                window.demand_chat(&chat);
                assert!(!next(&window).await.need_bytes);
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    large,
                    "disk bytes stay original"
                );
                chats.push(chat);
            }
            let epoch = window.demand_epoch();
            for _ in 0..3 {
                for chat in &chats {
                    assert!(get_avatar_image(chat.cache_key.as_ref().unwrap()).is_some());
                }
                window.demand_chats_windowed(&chats[..2], &chats[2..]);
                assert!(window.take_demands().is_empty());
                assert_eq!(window.demand_epoch(), epoch, "recovery must settle");
            }

            // Force a local decode to wait behind occupied workers, then
            // retire it. Its late result must not resurrect cleared images,
            // select an old picture, or send freshness for a departed account.
            for retirement in ["clear", "replacement", "account"] {
                clear_image_sources();
                let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (started, receiving) = std::sync::mpsc::channel();
                let mut blockers = Vec::new();
                let completed = native::DECODE_COMPLETED.load(std::sync::atomic::Ordering::Relaxed);
                let performed = native::DECODE_PERFORMED.load(std::sync::atomic::Ordering::Relaxed);
                for _ in 0..2 {
                    let release = release.clone();
                    let started = started.clone();
                    blockers.push(smol::spawn(native::decode_work(
                        decode_priority(DemandPriority::High),
                        move || {
                            started.send(()).unwrap();
                            while !release.load(std::sync::atomic::Ordering::Relaxed) {
                                std::thread::sleep(std::time::Duration::from_millis(1));
                            }
                        },
                    )));
                }
                for _ in 0..2 {
                    receiving
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                }
                let retired = AvatarManager::new();
                let reads = native::PERSISTENT_READS.load(std::sync::atomic::Ordering::Relaxed);
                if retirement == "replacement" {
                    retired.demand_one(&chats[0], DemandPriority::Low);
                } else {
                    retired.demand_chats_windowed(&chats[..1], &chats[1..2]);
                }
                let deadline = Instant::now() + std::time::Duration::from_secs(5);
                while native::queued_decodes() == 0 {
                    assert!(Instant::now() < deadline);
                    crate::platform::sleep(std::time::Duration::from_millis(1)).await;
                }
                retired.demand_chat(&chats[0]);
                assert_eq!(
                    native::PERSISTENT_READS.load(std::sync::atomic::Ordering::Relaxed),
                    reads + 1,
                    "promotion must not reread disk"
                );
                assert!(
                    retired
                        .persistent_in_flight
                        .lock()
                        .unwrap()
                        .values()
                        .all(|read| read.priority.load(Ordering::Relaxed)),
                    "visible demand must promote queued recovery"
                );
                assert!(
                    !retired.is_in_flight(&chats[1].jid),
                    "overscan must wait for visible local recovery"
                );
                match retirement {
                    "clear" => retired.clear(),
                    "replacement" => retired.on_avatar_ready(&chats[0].jid, "new-picture"),
                    "account" => retired.set_session(None),
                    _ => unreachable!(),
                }
                release.store(true, std::sync::atomic::Ordering::Relaxed);
                for blocker in blockers {
                    blocker.await;
                }
                while native::DECODE_COMPLETED.load(std::sync::atomic::Ordering::Relaxed)
                    < completed + 3
                {
                    assert!(Instant::now() < deadline);
                    crate::platform::sleep(std::time::Duration::from_millis(1)).await;
                }
                assert!(
                    get_avatar_image(chats[0].cache_key.as_ref().unwrap()).is_none(),
                    "late decode survived {retirement}"
                );
                assert!(retired.take_demands().is_empty());
                assert_eq!(
                    native::DECODE_PERFORMED.load(std::sync::atomic::Ordering::Relaxed),
                    performed,
                    "retired queued work must skip decoding after {retirement}"
                );
            }
        });
    }

    #[test]
    fn demands_deduplicate_in_flight() {
        let manager = AvatarManager::new();
        let chat = ChatDemand {
            jid: "user1@s.whatsapp.net".to_string(),
            picture_id: None,
            cache_key: None,
        };

        manager.demand_chat(&chat);
        assert!(manager.is_in_flight("user1@s.whatsapp.net"));

        // Second demand while first is in-flight should be deduplicated
        manager.demand_chat(&chat);
        let demands = manager.take_demands();
        assert_eq!(demands.len(), 1);
        assert_eq!(demands[0].jid, "user1@s.whatsapp.net");
        assert!(demands[0].need_bytes);

        // After completion, it can be demanded again
        manager.on_avatar_ready("user1@s.whatsapp.net", "");
        assert!(!manager.is_in_flight("user1@s.whatsapp.net"));
    }

    #[test]
    fn upgrading_demand_to_need_bytes_clears_known_picture_id() {
        let manager = AvatarManager::new();

        manager.enqueue_demand(
            AvatarDemand {
                jid: "user2@s.whatsapp.net".to_string(),
                known_picture_id: Some("pic-12345".to_string()),
                cache_key: Some("a-oldkey".to_string()),
                need_bytes: false,
                cache_miss: false,
            },
            DemandPriority::Low,
        );

        // Upgrade demand because persistent blob is missing or corrupt
        manager.enqueue_demand(
            AvatarDemand {
                jid: "user2@s.whatsapp.net".to_string(),
                known_picture_id: None,
                cache_key: None,
                need_bytes: true,
                cache_miss: true,
            },
            DemandPriority::High,
        );

        let demands = manager.take_demands();
        assert_eq!(demands.len(), 1);
        assert_eq!(demands[0].jid, "user2@s.whatsapp.net");
        assert!(demands[0].need_bytes);
        assert!(demands[0].cache_miss);
        assert_eq!(
            demands[0].known_picture_id, None,
            "must clear known_picture_id to avoid the Unchanged trap"
        );
    }

    #[test]
    fn no_photo_results_release_admission_and_are_negative_cached_for_the_connection() {
        let manager = AvatarManager::new();
        let absent = ChatDemand {
            jid: "absent@example.invalid".into(),
            picture_id: None,
            cache_key: None,
        };
        manager.demand_chat(&absent);
        let demands = manager.take_demands();
        assert_eq!(demands.len(), 1);
        assert!(
            demands[0].cache_miss,
            "an absent descriptor must supersede stale daemon readiness"
        );
        manager.on_avatar_ready(&absent.jid, "");
        manager.demand_chat(&absent);
        assert!(!manager.is_in_flight(&absent.jid));
        assert!(manager.take_demands().is_empty());
        for i in 0..(MAX_PENDING_DEMANDS + 32) {
            let chat = ChatDemand {
                jid: format!("absent-{i}@example.invalid"),
                picture_id: None,
                cache_key: None,
            };
            manager.demand_chat(&chat);
            assert_eq!(manager.take_demands().len(), 1);
            manager.on_avatar_ready(&chat.jid, "");
            manager.demand_chat(&chat);
        }
        assert!(manager.network_in_flight.lock().unwrap().is_empty());
        manager.reset_connection();
        manager.demand_chat(&absent);
        assert_eq!(manager.take_demands().len(), 1);
    }

    #[test]
    fn avatar_tracking_and_pending_admission_have_fixed_limits() {
        let manager = AvatarManager::new();
        for i in 0..(MAX_TRACKED_AVATARS + 32) {
            let jid = format!("synthetic-{i}@example.invalid");
            manager.on_avatar_failed(&jid, true);
            note_revalidation(
                &mut manager.revalidated.lock().unwrap(),
                &format!("revalidated-{i}@example.invalid"),
            );
        }
        assert_eq!(
            manager.revalidated.lock().unwrap().len(),
            MAX_TRACKED_AVATARS
        );
        assert_eq!(
            manager.failed_cooldowns.lock().unwrap().len(),
            MAX_TRACKED_AVATARS
        );
        for i in 0..(MAX_PENDING_DEMANDS + 32) {
            manager.demand_chat(&ChatDemand {
                jid: format!("pending-{i}@example.invalid"),
                picture_id: None,
                cache_key: None,
            });
        }
        assert_eq!(
            manager.pending_demands.lock().unwrap().len(),
            MAX_PENDING_DEMANDS
        );
        assert_eq!(
            manager.network_in_flight.lock().unwrap().len(),
            MAX_PENDING_DEMANDS
        );
        for demand in manager.take_demands() {
            manager.on_avatar_ready(&demand.jid, "");
        }
        manager.demand_chat(&ChatDemand {
            jid: "after-drain@example.invalid".into(),
            picture_id: None,
            cache_key: None,
        });
        assert_eq!(manager.take_demands().len(), 1);
    }

    #[test]
    fn replacement_and_removal_retire_older_persistent_reads_only_for_that_chat() {
        for key in ["new-picture", ""] {
            let manager = AvatarManager::new();
            let mut pending = manager.persistent_in_flight.lock().unwrap();
            pending.insert(
                ("peer@example.invalid".into(), "old-picture".into()),
                PendingRead::new(1, DemandPriority::Low),
            );
            pending.insert(
                ("other@example.invalid".into(), "other-picture".into()),
                PendingRead::new(2, DemandPriority::Low),
            );
            drop(pending);
            manager.on_avatar_ready("peer@example.invalid", key);
            let pending = manager.persistent_in_flight.lock().unwrap();
            assert!(!pending.contains_key(&("peer@example.invalid".into(), "old-picture".into())));
            assert!(
                pending.contains_key(&("other@example.invalid".into(), "other-picture".into()))
            );
        }
    }

    #[test]
    fn prioritized_demands_ordering() {
        let manager = AvatarManager::new();
        manager.enqueue_demand(
            AvatarDemand {
                jid: "overscan@s.whatsapp.net".to_string(),
                known_picture_id: None,
                cache_key: None,
                need_bytes: true,
                cache_miss: false,
            },
            DemandPriority::Low,
        );
        manager.enqueue_demand(
            AvatarDemand {
                jid: "visible@s.whatsapp.net".to_string(),
                known_picture_id: None,
                cache_key: None,
                need_bytes: true,
                cache_miss: false,
            },
            DemandPriority::High,
        );

        let demands = manager.take_demands();
        assert_eq!(demands.len(), 2);
        assert_eq!(demands[0].jid, "visible@s.whatsapp.net");
        assert_eq!(demands[1].jid, "overscan@s.whatsapp.net");
    }

    #[test]
    fn failure_cooldown_prevents_immediate_redemand() {
        let manager = AvatarManager::new();
        let chat = ChatDemand {
            jid: "failing@s.whatsapp.net".to_string(),
            picture_id: None,
            cache_key: None,
        };

        manager.demand_chat(&chat);
        assert!(manager.is_in_flight("failing@s.whatsapp.net"));
        assert_eq!(manager.take_demands().len(), 1);

        // Notify failure
        manager.on_avatar_failed("failing@s.whatsapp.net", true);
        assert!(!manager.is_in_flight("failing@s.whatsapp.net"));

        // Immediate subsequent demand must be throttled by cooldown
        manager.demand_chat(&chat);
        assert!(!manager.is_in_flight("failing@s.whatsapp.net"));
        assert!(manager.take_demands().is_empty());

        // on_avatar_ready clears cooldown
        manager.on_avatar_ready("failing@s.whatsapp.net", "a-synthetic-ready");
        manager.demand_chat(&chat);
        assert!(manager.is_in_flight("failing@s.whatsapp.net"));
        assert_eq!(manager.take_demands().len(), 1);
    }

    #[test]
    fn chunking_demands_to_64() {
        let manager = AvatarManager::new();
        for i in 0..130 {
            manager.enqueue_demand(
                AvatarDemand {
                    jid: format!("user{}@s.whatsapp.net", i),
                    known_picture_id: None,
                    cache_key: None,
                    need_bytes: true,
                    cache_miss: false,
                },
                DemandPriority::Low,
            );
        }

        let demands = manager.take_demands();
        assert_eq!(demands.len(), 130);
        let chunks: Vec<_> = demands.chunks(64).collect();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 64);
        assert_eq!(chunks[1].len(), 64);
        assert_eq!(chunks[2].len(), 2);
    }

    #[test]
    fn clear_resets_manager_state() {
        let manager = AvatarManager::new();
        manager.enqueue_demand(
            AvatarDemand {
                jid: "user3@s.whatsapp.net".to_string(),
                known_picture_id: None,
                cache_key: None,
                need_bytes: true,
                cache_miss: false,
            },
            DemandPriority::High,
        );
        manager
            .network_in_flight
            .lock()
            .unwrap()
            .insert("user3@s.whatsapp.net".to_string());
        manager
            .revalidated
            .lock()
            .unwrap()
            .insert("user3@s.whatsapp.net".to_string());

        manager.clear();

        assert!(!manager.is_in_flight("user3@s.whatsapp.net"));
        assert!(!manager.is_revalidated("user3@s.whatsapp.net"));
        assert!(manager.take_demands().is_empty());
    }

    #[test]
    fn reset_connection_clears_in_flight_and_revalidated() {
        let manager = AvatarManager::new();
        manager.enqueue_demand(
            AvatarDemand {
                jid: "user4@s.whatsapp.net".to_string(),
                known_picture_id: None,
                cache_key: None,
                need_bytes: true,
                cache_miss: false,
            },
            DemandPriority::High,
        );
        manager
            .network_in_flight
            .lock()
            .unwrap()
            .insert("user4@s.whatsapp.net".to_string());
        manager
            .revalidated
            .lock()
            .unwrap()
            .insert("user4@s.whatsapp.net".to_string());

        manager.reset_connection();

        assert!(!manager.is_in_flight("user4@s.whatsapp.net"));
        assert!(!manager.is_revalidated("user4@s.whatsapp.net"));
        assert!(manager.take_demands().is_empty());
    }

    #[test]
    fn viewport_overscan_windowing_bounds() {
        // Test visible windowing bounds calculation logic (+/- 10 rows)
        let total_rows: usize = 500;
        let visible_range: std::ops::Range<usize> = 15..30;

        let start = visible_range.start.saturating_sub(10);
        let end = (visible_range.end + 10).min(total_rows);

        assert_eq!(start, 5);
        assert_eq!(end, 40);
        assert_eq!(end - start, 35, "only 35 chats demanded out of 500");

        // Edge case at top
        let top_range: std::ops::Range<usize> = 0..10;
        let start_top = top_range.start.saturating_sub(10);
        let end_top = (top_range.end + 10).min(total_rows);
        assert_eq!(start_top, 0);
        assert_eq!(end_top, 20);

        // Edge case at bottom
        let bot_range: std::ops::Range<usize> = 495..500;
        let start_bot = bot_range.start.saturating_sub(10);
        let end_bot = (bot_range.end + 10).min(total_rows);
        assert_eq!(start_bot, 485);
        assert_eq!(end_bot, 500);
    }

    #[test]
    fn scrolling_back_requeues_an_unsent_overscan_avatar() {
        let manager = AvatarManager::new();
        let chat = ChatDemand {
            jid: "scroll@example.invalid".to_string(),
            picture_id: None,
            cache_key: None,
        };
        manager.demand_chats_windowed(&[], std::slice::from_ref(&chat));
        assert!(manager.is_in_flight(&chat.jid));
        manager.demand_chats_windowed(&[], &[]);
        assert!(!manager.is_in_flight(&chat.jid));
        assert!(manager.take_demands().is_empty());

        manager.demand_chats_windowed(std::slice::from_ref(&chat), &[]);
        let demands = manager.take_demands();
        assert_eq!(demands.len(), 1, "remount must retry the cancelled request");
        assert_eq!(demands[0].jid, chat.jid);
        assert!(demands[0].need_bytes);

        // Once dispatched, scrolling away must not cancel an active fetch.
        manager.demand_chats_windowed(&[], &[]);
        manager.demand_chat(&chat);
        assert!(manager.take_demands().is_empty());
        manager.on_avatar_ready(&chat.jid, "");
        assert!(!manager.is_in_flight(&chat.jid));
    }

    #[test]
    fn windowed_demand_prioritization_and_overscan_pruning() {
        let manager = AvatarManager::new();
        let chat_vis1 = ChatDemand {
            jid: "vis1@s.whatsapp.net".to_string(),
            picture_id: None,
            cache_key: None,
        };
        let chat_vis2 = ChatDemand {
            jid: "vis2@s.whatsapp.net".to_string(),
            picture_id: None,
            cache_key: None,
        };
        let chat_over1 = ChatDemand {
            jid: "over1@s.whatsapp.net".to_string(),
            picture_id: None,
            cache_key: None,
        };
        let chat_over2 = ChatDemand {
            jid: "over2@s.whatsapp.net".to_string(),
            picture_id: None,
            cache_key: None,
        };

        // Demand initial window: vis1, vis2 visible; over1, over2 overscan
        manager.demand_chats_windowed(
            &[chat_vis1.clone(), chat_vis2.clone()],
            &[chat_over1.clone(), chat_over2.clone()],
        );

        // Verify initial pending demands
        {
            let pending = manager.pending_demands.lock().unwrap();
            assert_eq!(pending.len(), 4);
            assert_eq!(
                pending.get("vis1@s.whatsapp.net").unwrap().1,
                DemandPriority::High
            );
            assert_eq!(
                pending.get("vis2@s.whatsapp.net").unwrap().1,
                DemandPriority::High
            );
            assert_eq!(
                pending.get("over1@s.whatsapp.net").unwrap().1,
                DemandPriority::Low
            );
            assert_eq!(
                pending.get("over2@s.whatsapp.net").unwrap().1,
                DemandPriority::Low
            );
        }

        // Fast scroll: window moves down to chat_over2 and a new chat_vis3.
        // over1 is no longer in visible or overscan!
        let chat_vis3 = ChatDemand {
            jid: "vis3@s.whatsapp.net".to_string(),
            picture_id: None,
            cache_key: None,
        };
        manager.demand_chats_windowed(
            std::slice::from_ref(&chat_vis3),
            std::slice::from_ref(&chat_over2),
        );

        // over1 (Low priority and not in new active window) must have been pruned.
        // vis1 and vis2 (High priority) are preserved.
        // over2 is retained because it's still in overscan.
        // vis3 is added as High priority.
        {
            let pending = manager.pending_demands.lock().unwrap();
            assert!(
                !pending.contains_key("over1@s.whatsapp.net"),
                "stale overscan demand must be pruned"
            );
            assert!(
                pending.contains_key("vis1@s.whatsapp.net"),
                "high priority demand is preserved"
            );
            assert!(
                pending.contains_key("vis2@s.whatsapp.net"),
                "high priority demand is preserved"
            );
            assert!(
                pending.contains_key("over2@s.whatsapp.net"),
                "active overscan demand is retained"
            );
            assert!(
                pending.contains_key("vis3@s.whatsapp.net"),
                "new visible demand is added"
            );
        }

        // take_demands must return all High priority first, followed by Low priority
        let demands = manager.take_demands();
        assert_eq!(demands.len(), 4);
        let high_jids: HashSet<_> = demands[..3].iter().map(|d| d.jid.as_str()).collect();
        assert!(high_jids.contains("vis1@s.whatsapp.net"));
        assert!(high_jids.contains("vis2@s.whatsapp.net"));
        assert!(high_jids.contains("vis3@s.whatsapp.net"));
        assert_eq!(demands[3].jid, "over2@s.whatsapp.net");
    }

    #[test]
    fn account_and_session_switch_invalidates_generation() {
        let manager = AvatarManager::new();
        let initial_gen = manager.generation.load(Ordering::Relaxed);
        assert_eq!(initial_gen, 0);

        let chat = ChatDemand {
            jid: "stale@s.whatsapp.net".to_string(),
            picture_id: None,
            cache_key: None,
        };
        manager.demand_chat(&chat);
        assert!(manager.is_in_flight("stale@s.whatsapp.net"));

        // Simulate session switch / account clear
        manager.set_session(None);
        let next_gen = manager.generation.load(Ordering::Relaxed);
        assert!(
            next_gen > initial_gen,
            "generation must increment on session switch"
        );
        assert!(
            !manager.is_in_flight("stale@s.whatsapp.net"),
            "in-flight must be cleared on session switch"
        );
        assert!(
            manager.take_demands().is_empty(),
            "pending demands must be cleared on session switch"
        );
    }

    #[test]
    fn failure_cooldown_retryable_vs_non_retryable() {
        let manager = AvatarManager::new();
        let jid_retryable = "retryable@s.whatsapp.net";
        let jid_terminal = "terminal@s.whatsapp.net";

        manager.on_avatar_failed(jid_retryable, true);
        manager.on_avatar_failed(jid_terminal, false);

        let cooldowns = manager.failed_cooldowns.lock().unwrap();
        let (retryable, until_retryable) = cooldowns.get(jid_retryable).unwrap();
        let (terminal, until_terminal) = cooldowns.get(jid_terminal).unwrap();

        assert!(*retryable);
        assert!(!*terminal);

        let now = Instant::now();
        // Retryable cooldown is ~15s
        assert!(*until_retryable > now + std::time::Duration::from_secs(10));
        assert!(*until_retryable <= now + std::time::Duration::from_secs(20));

        // Terminal cooldown is ~300s
        assert!(*until_terminal > now + std::time::Duration::from_secs(250));
        assert!(*until_terminal <= now + std::time::Duration::from_secs(310));
    }
}
