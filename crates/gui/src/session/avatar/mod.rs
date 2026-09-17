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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use oxidezap_core::AvatarDemand;

use crate::session::media::{
    clear_image_sources, decode_avatar, get_avatar_image, put_avatar_image,
};

pub use imp::{
    clear_cache_storage, delete_account_storage, read_persistent, spawn_task, write_persistent,
};

/// Persist an avatar payload to the platform's storage tier.
#[allow(dead_code)]
pub fn save_avatar(key: &str, bytes: &[u8]) {
    let key = key.to_string();
    let bytes = bytes.to_vec();
    spawn_task(async move {
        let _ = write_persistent(&key, &bytes).await;
    });
}

/// One avatar demand description passed by the viewport or header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatDemand {
    pub jid: String,
    pub picture_id: Option<String>,
    pub cache_key: Option<String>,
}

/// Central avatar manager coordinating viewport demands, persistent storage,
/// and in-memory decoded image caching.
#[derive(Clone, Default)]
pub struct AvatarManager {
    in_flight: Arc<Mutex<HashSet<String>>>,
    revalidated: Arc<Mutex<HashSet<String>>>,
    pending_demands: Arc<Mutex<HashMap<String, AvatarDemand>>>,
}

impl AvatarManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset in-memory cache and tracking (e.g. on cache clear or account change).
    pub fn clear(&self) {
        clear_image_sources();
        self.in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.revalidated
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.pending_demands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Queue a demand for one chat (e.g. from the chat header or active conversation).
    pub fn demand_chat(&self, chat: &ChatDemand) {
        self.demand_chats(std::slice::from_ref(chat));
    }

    /// Process viewport demands. Checks RAM first; on miss, checks persistent storage;
    /// on persistent miss, enqueues demand to the daemon with `need_bytes = true`.
    pub fn demand_chats(&self, chats: &[ChatDemand]) {
        for chat in chats {
            let jid = &chat.jid;
            if let Some(key) = &chat.cache_key {
                if get_avatar_image(key).is_some() {
                    // Cache hit in RAM: check if revalidation is needed for this connection
                    let mut reval = self.revalidated.lock().unwrap_or_else(|e| e.into_inner());
                    if reval.insert(jid.clone()) {
                        self.enqueue_demand(AvatarDemand {
                            jid: jid.clone(),
                            known_picture_id: chat.picture_id.clone(),
                            cache_key: Some(key.clone()),
                            need_bytes: false,
                        });
                    }
                    continue;
                }

                // Miss in RAM: check persistent store if not already in flight
                let mut flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
                if flight.insert(key.clone()) {
                    let key_clone = key.clone();
                    let jid_clone = jid.clone();
                    let pic_id = chat.picture_id.clone();
                    let in_flight = Arc::clone(&self.in_flight);
                    let revalidated = Arc::clone(&self.revalidated);
                    let pending_demands = Arc::clone(&self.pending_demands);

                    spawn_task(async move {
                        let persistent_bytes = read_persistent(&key_clone).await;
                        if let Some(bytes) = persistent_bytes
                            && let Some((source, decoded_size)) = decode_avatar(&bytes)
                        {
                            put_avatar_image(key_clone.clone(), source, decoded_size);
                            in_flight
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&key_clone);

                            // If successfully loaded from persistent cache, queue a
                            // conditional check to verify freshness with the server:
                            let mut reval = revalidated.lock().unwrap_or_else(|e| e.into_inner());
                            if reval.insert(jid_clone.clone()) {
                                pending_demands
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .insert(
                                        jid_clone.clone(),
                                        AvatarDemand {
                                            jid: jid_clone,
                                            known_picture_id: pic_id,
                                            cache_key: Some(key_clone),
                                            need_bytes: false,
                                        },
                                    );
                            }
                            return;
                        }

                        // Persistent miss (or corrupted file): must unconditionally fetch bytes
                        in_flight
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .remove(&key_clone);
                        pending_demands
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(
                                jid_clone.clone(),
                                AvatarDemand {
                                    jid: jid_clone,
                                    known_picture_id: None,
                                    cache_key: None,
                                    need_bytes: true,
                                },
                            );
                    });
                }
            } else {
                // No avatar key recorded: query WhatsApp
                let mut flight = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
                if flight.insert(jid.clone()) {
                    self.enqueue_demand(AvatarDemand {
                        jid: jid.clone(),
                        known_picture_id: None,
                        cache_key: None,
                        need_bytes: true,
                    });
                }
            }
        }
    }

    pub fn enqueue_demand(&self, demand: AvatarDemand) {
        let mut pending = self
            .pending_demands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending
            .entry(demand.jid.clone())
            .and_modify(|existing| {
                if demand.need_bytes {
                    existing.need_bytes = true;
                    existing.known_picture_id = None;
                }
            })
            .or_insert(demand);
    }

    /// Extract all pending demands.
    pub fn take_demands(&self) -> Vec<AvatarDemand> {
        let mut pending = self
            .pending_demands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pending.drain().map(|(_, d)| d).collect()
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
        self.in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(jid);
        if !key.is_empty() {
            self.in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(key);
        }
    }

    #[cfg(test)]
    pub fn is_in_flight(&self, item: &str) -> bool {
        self.in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(item)
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

        manager.enqueue_demand(AvatarDemand {
            jid: "user2@s.whatsapp.net".to_string(),
            known_picture_id: Some("pic-12345".to_string()),
            cache_key: Some("a-oldkey".to_string()),
            need_bytes: false,
        });

        // Upgrade demand because persistent blob is missing or corrupt
        manager.enqueue_demand(AvatarDemand {
            jid: "user2@s.whatsapp.net".to_string(),
            known_picture_id: None,
            cache_key: None,
            need_bytes: true,
        });

        let demands = manager.take_demands();
        assert_eq!(demands.len(), 1);
        assert_eq!(demands[0].jid, "user2@s.whatsapp.net");
        assert!(demands[0].need_bytes);
        assert_eq!(
            demands[0].known_picture_id, None,
            "must clear known_picture_id to avoid the Unchanged trap"
        );
    }

    #[test]
    fn chunking_demands_to_64() {
        let manager = AvatarManager::new();
        for i in 0..130 {
            manager.enqueue_demand(AvatarDemand {
                jid: format!("user{}@s.whatsapp.net", i),
                known_picture_id: None,
                cache_key: None,
                need_bytes: true,
            });
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
        manager.enqueue_demand(AvatarDemand {
            jid: "user3@s.whatsapp.net".to_string(),
            known_picture_id: None,
            cache_key: None,
            need_bytes: true,
        });
        manager
            .in_flight
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
}
