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

#[cfg(not(target_family = "wasm"))]
use native as imp;
#[cfg(target_family = "wasm")]
use web as imp;

use portable_atomic::{AtomicU64, Ordering};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use oxidezap_core::{AccountId, AvatarDemand};
use wacore::time::Instant;

use crate::session::media::{
    clear_image_sources, decode_avatar, get_avatar_image, put_avatar_image,
};

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
        spawn_task(async move {
            let _ = write_persistent(&key, &bytes, account).await;
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

static NEXT_OP_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Central avatar manager coordinating viewport demands, persistent storage,
/// and in-memory decoded image caching.
#[derive(Clone, Default)]
pub struct AvatarManager {
    persistent_in_flight: Arc<Mutex<HashMap<String, u64>>>,
    network_in_flight: Arc<Mutex<HashSet<String>>>,
    revalidated: Arc<Mutex<HashSet<String>>>,
    failed_cooldowns: Arc<Mutex<HashMap<String, (bool, Instant)>>>,
    pending_demands: Arc<Mutex<HashMap<String, (AvatarDemand, DemandPriority)>>>,
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
        self.demand_epoch.load(Ordering::Relaxed)
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
        clear_image_sources();
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.bump_demand_epoch();
        self.reset_connection();
    }

    /// Clear in-flight, revalidated, and cooldown tracking on connection restart while
    /// preserving decoded images in memory.
    pub fn reset_connection(&self) {
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
        for chat in overscan {
            self.demand_one(chat, DemandPriority::Low);
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
        pending.retain(|jid, (_demand, priority)| {
            *priority == DemandPriority::High || active_jids.contains(jid)
        });
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
                if reval.insert(jid.clone()) {
                    self.enqueue_demand(
                        AvatarDemand {
                            jid: jid.clone(),
                            known_picture_id: chat.picture_id.clone(),
                            cache_key: Some(key.clone()),
                            need_bytes: false,
                            cache_miss: false,
                        },
                        priority,
                    );
                }
                return;
            }

            // Miss in RAM: check persistent store if not already in flight
            let op_token = NEXT_OP_TOKEN.fetch_add(1, Ordering::Relaxed);
            let mut p_flight = self
                .persistent_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let std::collections::hash_map::Entry::Vacant(entry) = p_flight.entry(key.clone()) {
                entry.insert(op_token);
                let key_clone = key.clone();
                let jid_clone = jid.clone();
                let pic_id = chat.picture_id.clone();
                let persistent_in_flight = Arc::clone(&self.persistent_in_flight);
                let network_in_flight = Arc::clone(&self.network_in_flight);
                let revalidated = Arc::clone(&self.revalidated);
                let pending_demands = Arc::clone(&self.pending_demands);
                let session_handle = Arc::clone(&self.session);
                let generation = Arc::clone(&self.generation);
                let account_id = Arc::clone(&self.account_id);
                let born_gen = generation.load(Ordering::Relaxed);
                let born_account = *account_id.lock().unwrap_or_else(|e| e.into_inner());

                spawn_task(async move {
                    let persistent_bytes = read_persistent(&key_clone).await;

                    // Generation safety: discard if session/account changed while reading
                    if generation.load(Ordering::Relaxed) != born_gen
                        || *account_id.lock().unwrap_or_else(|e| e.into_inner()) != born_account
                    {
                        let mut p = persistent_in_flight
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if p.get(&key_clone) == Some(&op_token) {
                            p.remove(&key_clone);
                        }
                        return;
                    }

                    if let Some(bytes) = persistent_bytes {
                        if let Some((source, decoded_size)) = decode_avatar(&bytes) {
                            put_avatar_image(key_clone.clone(), source, decoded_size);
                            let mut p = persistent_in_flight
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            if p.get(&key_clone) == Some(&op_token) {
                                p.remove(&key_clone);
                            }

                            // Notify UI that the avatar image is ready in RAM
                            if let Some(session) = session_handle
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .as_ref()
                            {
                                session.notify_avatar_ready(jid_clone.clone(), key_clone.clone());
                            }

                            // Conditional freshness revalidation:
                            let mut reval = revalidated.lock().unwrap_or_else(|e| e.into_inner());
                            if reval.insert(jid_clone.clone()) {
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
                            if p.get(&key_clone) == Some(&op_token) {
                                p.remove(&key_clone);
                            }
                            return;
                        }
                    }

                    // Persistent miss or corrupt blob: unconditionally fetch bytes
                    {
                        let mut p = persistent_in_flight
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if p.get(&key_clone) == Some(&op_token) {
                            p.remove(&key_clone);
                        }
                    }
                    let mut n_flight = network_in_flight.lock().unwrap_or_else(|e| e.into_inner());
                    if n_flight.insert(jid_clone.clone()) {
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
            // No avatar key recorded: query WhatsApp
            let mut n_flight = self
                .network_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if n_flight.insert(jid.clone()) {
                self.enqueue_demand(
                    AvatarDemand {
                        jid: jid.clone(),
                        known_picture_id: None,
                        cache_key: None,
                        need_bytes: true,
                        cache_miss: false,
                    },
                    priority,
                );
            }
        }
    }

    pub fn enqueue_demand(&self, demand: AvatarDemand, priority: DemandPriority) {
        let mut pending = self
            .pending_demands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        self.network_in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(jid);
        if !key.is_empty() {
            self.persistent_in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(key);
        }
        self.failed_cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(jid);
    }

    /// Called when an avatar lookup, download, or materialization fails.
    pub fn on_avatar_failed(&self, jid: &str, retryable: bool) -> Instant {
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
        self.failed_cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(jid.to_string(), (retryable, until));
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
                .contains_key(item)
    }

    #[cfg(test)]
    pub fn is_revalidated(&self, jid: &str) -> bool {
        self.revalidated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(jid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        manager.on_avatar_ready("failing@s.whatsapp.net", "");
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
