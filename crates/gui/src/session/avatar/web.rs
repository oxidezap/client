//! Web implementation of persistent avatar storage via the browser Cache Storage API.
//!
//! Avatars survive browser refresh (F5) in the Cache Storage API (`window.caches`)
//! strictly scoped to the active account: `oxidezap-avatar-v2-a<account_id>`.
//! Keys are mapped to synthetic HTTP URLs (`<origin>/_oxidezap/avatar/<cache_key>`).
//!
//! Security and Isolation:
//! - No `"default"` namespace fallback. If account identity is unknown, persistent
//!   caching is safely bypassed for that operation.
//! - Caches from older versions (`oxidezap-avatar-v1-*`) are lazily cleaned on startup
//!   without touching foreign origin caches.
//! - Total cache budget is limited to ~64 MiB with LRU eviction of oldest entries,
//!   tracked via lightweight metadata in `localStorage`.
//!
//! Memory safety: Since the module is built with `--shared-memory`, browser APIs
//! cannot take direct views into wasm linear memory. Every byte slice crossing
//! into JS is copied into `js_sys::Uint8Array::from(...)`, and reads copy out
//! using `array.to_vec()`.

use std::sync::RwLock;
use wasm_bindgen::JsCast;

const AVATAR_CACHE_BUDGET_BYTES: u64 = 64 * 1024 * 1024; // 64 MiB

static ACTIVE_ACCOUNT: RwLock<Option<oxidezap_core::AccountId>> = RwLock::new(None);

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

pub fn resolve_account(key: &str) -> Option<oxidezap_core::AccountId> {
    oxidezap_ipc::account_staged_prefix_of(key).or_else(active_account)
}

pub fn cache_name_for_account(account: oxidezap_core::AccountId) -> String {
    format!("oxidezap-avatar-v2-a{}", account.get())
}

pub fn cache_url(key: &str) -> String {
    let origin = web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .filter(|o| !o.is_empty() && o != "null")
        .unwrap_or_else(|| "https://oxidezap.local".to_string());
    format!("{origin}/_oxidezap/avatar/{key}")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct CacheMeta {
    entries: std::collections::HashMap<String, MetaEntry>,
    total_bytes: u64,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct MetaEntry {
    size: u64,
    last_accessed: u64,
}

fn meta_storage_key(account: oxidezap_core::AccountId) -> String {
    format!("oxidezap_avatar_meta_v2_a{}", account.get())
}

fn load_meta(account: oxidezap_core::AccountId) -> CacheMeta {
    let Some(window) = web_sys::window() else {
        return CacheMeta::default();
    };
    let Ok(Some(storage)) = window.local_storage() else {
        return CacheMeta::default();
    };
    let key = meta_storage_key(account);
    if let Ok(Some(json)) = storage.get_item(&key) {
        if let Ok(meta) = serde_json::from_str::<CacheMeta>(&json) {
            return meta;
        }
    }
    CacheMeta::default()
}

fn save_meta(account: oxidezap_core::AccountId, meta: &CacheMeta) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(Some(storage)) = window.local_storage() else {
        return;
    };
    let key = meta_storage_key(account);
    if let Ok(json) = serde_json::to_string(meta) {
        let _ = storage.set_item(&key, &json);
    }
}

pub async fn read_persistent(key: &str) -> Option<Vec<u8>> {
    let account = resolve_account(key)?;
    let window = web_sys::window()?;
    let caches = window.caches().ok()?;
    let name = cache_name_for_account(account);
    let cache_promise = caches.open(&name);
    let cache_val = wasm_bindgen_futures::JsFuture::from(cache_promise)
        .await
        .ok()?;
    let cache: web_sys::Cache = cache_val.dyn_into().ok()?;
    let url = cache_url(key);
    let match_promise = cache.match_with_str(&url);
    let response_val = wasm_bindgen_futures::JsFuture::from(match_promise)
        .await
        .ok()?;
    if response_val.is_undefined() || response_val.is_null() {
        return None;
    }
    let response: web_sys::Response = response_val.dyn_into().ok()?;
    let buffer_promise = response.array_buffer().ok()?;
    let buffer = wasm_bindgen_futures::JsFuture::from(buffer_promise)
        .await
        .ok()?;
    let array = js_sys::Uint8Array::new(&buffer);
    let bytes = array.to_vec();

    // Update last_accessed in metadata
    let mut meta = load_meta(account);
    let now = wacore::time::now_millis();
    let now_u64 = if now >= 0 { now as u64 } else { 0 };
    if let Some(entry) = meta.entries.get_mut(key) {
        entry.last_accessed = now_u64;
    } else {
        meta.entries.insert(
            key.to_string(),
            MetaEntry {
                size: bytes.len() as u64,
                last_accessed: now_u64,
            },
        );
        meta.total_bytes = meta.total_bytes.saturating_add(bytes.len() as u64);
    }
    save_meta(account, &meta);

    Some(bytes)
}

pub async fn delete_persistent(key: &str) -> Result<(), String> {
    let Some(account) = resolve_account(key) else {
        return Ok(());
    };
    let window = web_sys::window().ok_or_else(|| "no window".to_string())?;
    let caches = window.caches().map_err(|e| format!("{e:?}"))?;
    let name = cache_name_for_account(account);
    let cache_promise = caches.open(&name);
    let cache_val = wasm_bindgen_futures::JsFuture::from(cache_promise)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let cache: web_sys::Cache = cache_val.dyn_into().map_err(|e| format!("{e:?}"))?;
    let url = cache_url(key);
    let _ = wasm_bindgen_futures::JsFuture::from(cache.delete_with_str(&url)).await;

    let mut meta = load_meta(account);
    if let Some(removed) = meta.entries.remove(key) {
        meta.total_bytes = meta.total_bytes.saturating_sub(removed.size);
        save_meta(account, &meta);
    }
    Ok(())
}

pub async fn write_persistent(
    key: &str,
    bytes: &[u8],
    account: Option<oxidezap_core::AccountId>,
) -> Result<(), String> {
    let target_account = account.or_else(|| resolve_account(key));
    let Some(target_account) = target_account else {
        log::debug!("skipping persistent avatar write: no account scope for key {key}");
        return Ok(());
    };
    if let Some(active) = active_account() {
        if active != target_account {
            log::debug!("skipping persistent avatar write for inactive account");
            return Ok(());
        }
    }

    let window = web_sys::window().ok_or_else(|| "no window".to_string())?;
    let caches = window.caches().map_err(|e| format!("{e:?}"))?;
    let name = cache_name_for_account(target_account);
    let cache_promise = caches.open(&name);
    let cache_val = wasm_bindgen_futures::JsFuture::from(cache_promise)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let cache: web_sys::Cache = cache_val.dyn_into().map_err(|e| format!("{e:?}"))?;

    let new_size = bytes.len() as u64;
    let mut meta = load_meta(target_account);

    // Evict oldest entries if total budget exceeds AVATAR_CACHE_BUDGET_BYTES
    if meta.total_bytes.saturating_add(new_size) > AVATAR_CACHE_BUDGET_BYTES {
        let mut sorted_entries: Vec<(String, u64, u64)> = meta
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.size, v.last_accessed))
            .collect();
        sorted_entries.sort_by_key(|(_, _, last_accessed)| *last_accessed);

        for (evicted_key, size, _) in sorted_entries {
            if meta.total_bytes.saturating_add(new_size) <= AVATAR_CACHE_BUDGET_BYTES {
                break;
            }
            if evicted_key == key {
                continue;
            }
            let evict_url = cache_url(&evicted_key);
            let _ = wasm_bindgen_futures::JsFuture::from(cache.delete_with_str(&evict_url)).await;
            meta.entries.remove(&evicted_key);
            meta.total_bytes = meta.total_bytes.saturating_sub(size);
        }
    }

    let url = cache_url(key);
    // Copy bytes into a JS-owned Uint8Array buffer and construct a Blob
    let js_array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::new();
    parts.push(&js_array.buffer());
    let blob = web_sys::Blob::new_with_u8_array_sequence(&parts).map_err(|e| format!("{e:?}"))?;
    let response =
        web_sys::Response::new_with_opt_blob(Some(&blob)).map_err(|e| format!("{e:?}"))?;
    let put_promise = cache.put_with_str(&url, &response);
    wasm_bindgen_futures::JsFuture::from(put_promise)
        .await
        .map_err(|e| format!("{e:?}"))?;

    let now = wacore::time::now_millis();
    let now_u64 = if now >= 0 { now as u64 } else { 0 };
    if let Some(existing) = meta.entries.insert(
        key.to_string(),
        MetaEntry {
            size: new_size,
            last_accessed: now_u64,
        },
    ) {
        meta.total_bytes = meta.total_bytes.saturating_sub(existing.size);
    }
    meta.total_bytes = meta.total_bytes.saturating_add(new_size);
    save_meta(target_account, &meta);

    Ok(())
}

pub async fn delete_account_storage(account: Option<oxidezap_core::AccountId>) {
    let Some(account) = account else {
        return;
    };
    let name = cache_name_for_account(account);
    if let Some(window) = web_sys::window() {
        if let Ok(caches) = window.caches() {
            let _ = wasm_bindgen_futures::JsFuture::from(caches.delete(&name)).await;
        }
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.remove_item(&meta_storage_key(account));
        }
    }
}

pub async fn clear_cache_storage(account: Option<oxidezap_core::AccountId>) {
    let target = account.or_else(active_account);
    delete_account_storage(target).await;
}

pub async fn avatar_cache_usage(account: oxidezap_core::AccountId) -> (u64, u64) {
    let meta = load_meta(account);
    (meta.total_bytes, meta.entries.len() as u64)
}

/// Lazily delete deprecated v1 avatar caches without touching foreign origin caches.
pub async fn purge_legacy_caches() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(caches) = window.caches() else {
        return;
    };
    let Ok(keys_val) = wasm_bindgen_futures::JsFuture::from(caches.keys()).await else {
        return;
    };
    let Ok(array) = keys_val.dyn_into::<js_sys::Array>() else {
        return;
    };
    for i in 0..array.length() {
        if let Some(name) = array.get(i).as_string() {
            if name.starts_with("oxidezap-avatar-v1-") {
                let _ = wasm_bindgen_futures::JsFuture::from(caches.delete(&name)).await;
            }
        }
    }
}

pub fn spawn_task(fut: impl std::future::Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(fut);
}
