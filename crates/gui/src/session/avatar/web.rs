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
//! - The cache has byte and entry limits, evicting oldest stored entries,
//!   tracked via lightweight metadata in `localStorage`.
//!
//! Memory safety: Since the module is built with `--shared-memory`, browser APIs
//! cannot take direct views into wasm linear memory. Every byte slice crossing
//! into JS is copied into `js_sys::Uint8Array::from(...)`, and reads copy out
//! using `array.to_vec()`.

use std::sync::RwLock;
use wasm_bindgen::JsCast;

const AVATAR_CACHE_BUDGET_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 2048;
const MAX_METADATA_BYTES: usize = 1024 * 1024;
static STORAGE_GENERATION: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

static ACTIVE_ACCOUNT: RwLock<Option<oxidezap_core::AccountId>> = RwLock::new(None);

pub(super) async fn decode_avatar(
    bytes: Vec<u8>,
    _priority: super::DecodePriority,
    current: impl FnOnce() -> bool + Send + 'static,
) -> Option<(std::sync::Arc<gpui::RenderImage>, u64)> {
    current()
        .then(|| crate::session::media::decode_avatar(&bytes))
        .flatten()
}

pub fn set_active_account(account: Option<oxidezap_core::AccountId>) {
    if let Ok(mut lock) = ACTIVE_ACCOUNT.write() {
        if *lock != account {
            STORAGE_GENERATION.fetch_add(1, portable_atomic::Ordering::Relaxed);
        }
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
    oxidezap_ipc::account_id_of(key).or_else(active_account)
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

fn load_meta(account: oxidezap_core::AccountId) -> Option<CacheMeta> {
    let Some(window) = web_sys::window() else {
        return None;
    };
    let Ok(Some(storage)) = window.local_storage() else {
        return None;
    };
    let key = meta_storage_key(account);
    if let Ok(Some(json)) = storage.get_item(&key) {
        if json.len() <= MAX_METADATA_BYTES {
            if let Ok(mut meta) = serde_json::from_str::<CacheMeta>(&json) {
                if meta.entries.len() <= MAX_CACHE_ENTRIES {
                    meta.total_bytes = meta
                        .entries
                        .values()
                        .fold(0u64, |sum, entry| sum.saturating_add(entry.size));
                    return Some(meta);
                }
            }
        }
    }
    None
}

fn storage() -> Result<web_sys::Storage, String> {
    web_sys::window()
        .ok_or("no window")?
        .local_storage()
        .map_err(|e| format!("{e:?}"))?
        .ok_or_else(|| "localStorage unavailable".into())
}

fn save_meta(account: oxidezap_core::AccountId, meta: &CacheMeta) -> Result<(), String> {
    let json = serde_json::to_string(meta).map_err(|e| e.to_string())?;
    storage()?
        .set_item(&meta_storage_key(account), &json)
        .map_err(|e| format!("{e:?}"))
}

pub fn persistent_stamp(account: Option<oxidezap_core::AccountId>) -> Result<u64, String> {
    let account = account.ok_or("no account")?;
    storage()?
        .get_item(&format!("{}_generation", meta_storage_key(account)))
        .map_err(|e| format!("{e:?}"))?
        .map_or(Ok(0), |value| {
            value.parse().map_err(|_| "invalid cache generation".into())
        })
}

fn dirty_key(account: oxidezap_core::AccountId) -> String {
    format!("{}_dirty", meta_storage_key(account))
}

fn mark_dirty(account: oxidezap_core::AccountId) -> Result<(), String> {
    storage()?
        .set_item(&dirty_key(account), "1")
        .map_err(|e| format!("{e:?}"))
}

fn finish_meta(account: oxidezap_core::AccountId, meta: &CacheMeta) -> Result<(), String> {
    save_meta(account, meta)?;
    storage()?
        .remove_item(&dirty_key(account))
        .map_err(|e| format!("{e:?}"))
}

thread_local! {
    // One remembered account, so visiting old accounts cannot grow a registry.
    static RECONCILED: std::cell::Cell<Option<oxidezap_core::AccountId>> = const { std::cell::Cell::new(None) };
}

/// Recover once per account opening, and after an interrupted mutation. Keys and
/// blob sizes are authoritative; localStorage is only a bounded derived index.
async fn ensure_meta(
    account: oxidezap_core::AccountId,
    cache: &web_sys::Cache,
) -> Result<CacheMeta, String> {
    let old = load_meta(account);
    let dirty = storage()?
        .get_item(&dirty_key(account))
        .map_err(|e| format!("{e:?}"))?
        .is_some();
    if RECONCILED.with(|current| current.get() == Some(account)) && !dirty {
        if let Some(meta) = old {
            return Ok(meta);
        }
    }
    mark_dirty(account)?;
    let keys = wasm_bindgen_futures::JsFuture::from(cache.keys())
        .await
        .map_err(|e| format!("{e:?}"))?;
    let keys = js_sys::Array::from(&keys);
    let prefix = cache_url("");
    let mut meta = CacheMeta::default();
    for i in 0..keys.length() {
        let request = keys
            .get(i)
            .dyn_into::<web_sys::Request>()
            .map_err(|e| format!("{e:?}"))?;
        let url = request.url();
        let Some(key) = url.strip_prefix(&prefix) else {
            continue;
        };
        let response = wasm_bindgen_futures::JsFuture::from(cache.match_with_request(&request))
            .await
            .map_err(|e| format!("{e:?}"))?;
        if response.is_undefined() {
            continue;
        }
        let response = response
            .dyn_into::<web_sys::Response>()
            .map_err(|e| format!("{e:?}"))?;
        let blob =
            wasm_bindgen_futures::JsFuture::from(response.blob().map_err(|e| format!("{e:?}"))?)
                .await
                .map_err(|e| format!("{e:?}"))?;
        let size = blob
            .dyn_into::<web_sys::Blob>()
            .map_err(|e| format!("{e:?}"))?
            .size() as u64;
        let last_accessed = old
            .as_ref()
            .and_then(|old| old.entries.get(key))
            .map_or(u64::from(i), |entry| entry.last_accessed);
        meta.entries.insert(
            key.to_string(),
            MetaEntry {
                size,
                last_accessed,
            },
        );
        meta.total_bytes = meta.total_bytes.saturating_add(size);
        while meta.total_bytes > AVATAR_CACHE_BUDGET_BYTES || meta.entries.len() > MAX_CACHE_ENTRIES
        {
            let oldest = meta
                .entries
                .iter()
                .min_by_key(|(key, entry)| (entry.last_accessed, *key))
                .map(|(key, _)| key.clone())
                .ok_or("empty cache index")?;
            wasm_bindgen_futures::JsFuture::from(cache.delete_with_str(&cache_url(&oldest)))
                .await
                .map_err(|e| format!("{e:?}"))?;
            if let Some(entry) = meta.entries.remove(&oldest) {
                meta.total_bytes = meta.total_bytes.saturating_sub(entry.size);
            }
        }
    }
    finish_meta(account, &meta)?;
    RECONCILED.with(|current| current.set(Some(account)));
    Ok(meta)
}

pub async fn read_persistent(key: &str) -> Option<Vec<u8>> {
    let account = resolve_account(key)?;
    if active_account() != Some(account) {
        return None;
    }
    let origin = super::web_lock::lock().await.ok();
    let window = web_sys::window()?;
    let caches = window.caches().ok()?;
    let name = cache_name_for_account(account);
    let cache_promise = caches.open(&name);
    let cache_val = wasm_bindgen_futures::JsFuture::from(cache_promise)
        .await
        .ok()?;
    let cache: web_sys::Cache = cache_val.dyn_into().ok()?;
    if origin.is_some() {
        // Recovery also runs for cache-only offline use. Failure to update
        // bookkeeping must not hide otherwise readable cached bytes.
        let _ = ensure_meta(account, &cache).await;
    }
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
    Some(array.to_vec())
}

pub async fn delete_persistent(key: &str) -> Result<(), String> {
    let Some(account) = resolve_account(key) else {
        return Ok(());
    };
    let _origin_guard = super::web_lock::lock().await?;
    let window = web_sys::window().ok_or_else(|| "no window".to_string())?;
    let caches = window.caches().map_err(|e| format!("{e:?}"))?;
    let name = cache_name_for_account(account);
    let cache_promise = caches.open(&name);
    let cache_val = wasm_bindgen_futures::JsFuture::from(cache_promise)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let cache: web_sys::Cache = cache_val.dyn_into().map_err(|e| format!("{e:?}"))?;
    let mut meta = ensure_meta(account, &cache).await?;
    mark_dirty(account)?;
    wasm_bindgen_futures::JsFuture::from(cache.delete_with_str(&cache_url(key)))
        .await
        .map_err(|e| format!("{e:?}"))?;
    if let Some(removed) = meta.entries.remove(key) {
        meta.total_bytes = meta.total_bytes.saturating_sub(removed.size);
    }
    finish_meta(account, &meta)?;
    Ok(())
}

pub async fn write_persistent(
    key: &str,
    bytes: &[u8],
    account: Option<oxidezap_core::AccountId>,
    stamp: u64,
) -> Result<(), String> {
    let generation = STORAGE_GENERATION.load(portable_atomic::Ordering::Relaxed);
    let target_account = account.or_else(|| resolve_account(key));
    let Some(target_account) = target_account else {
        log::debug!("skipping persistent avatar write: no account scope for key {key}");
        return Ok(());
    };
    if resolve_account(key) != Some(target_account) {
        return Err("avatar key belongs to another account".into());
    }
    if active_account() != Some(target_account) {
        log::debug!("skipping persistent avatar write for inactive account");
        return Ok(());
    }

    let _origin_guard = super::web_lock::lock().await?;

    let window = web_sys::window().ok_or_else(|| "no window".to_string())?;
    let caches = window.caches().map_err(|e| format!("{e:?}"))?;
    let name = cache_name_for_account(target_account);
    let cache_promise = caches.open(&name);
    let cache_val = wasm_bindgen_futures::JsFuture::from(cache_promise)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let cache: web_sys::Cache = cache_val.dyn_into().map_err(|e| format!("{e:?}"))?;

    if generation != STORAGE_GENERATION.load(portable_atomic::Ordering::Relaxed)
        || active_account() != Some(target_account)
    {
        return Ok(());
    }
    let new_size = bytes.len() as u64;
    if new_size > AVATAR_CACHE_BUDGET_BYTES {
        return Ok(());
    }
    if persistent_stamp(Some(target_account))? != stamp {
        return Ok(());
    }
    let mut meta = ensure_meta(target_account, &cache).await?;
    mark_dirty(target_account)?;
    if let Some(previous) = meta.entries.remove(key) {
        meta.total_bytes = meta.total_bytes.saturating_sub(previous.size);
    }

    // Evict oldest entries if total budget exceeds AVATAR_CACHE_BUDGET_BYTES
    if meta.total_bytes.saturating_add(new_size) > AVATAR_CACHE_BUDGET_BYTES
        || meta.entries.len() >= MAX_CACHE_ENTRIES
    {
        let mut sorted_entries: Vec<(String, u64, u64)> = meta
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.size, v.last_accessed))
            .collect();
        sorted_entries.sort_by_key(|(_, _, last_accessed)| *last_accessed);

        for (evicted_key, size, _) in sorted_entries {
            if meta.total_bytes.saturating_add(new_size) <= AVATAR_CACHE_BUDGET_BYTES
                && meta.entries.len() < MAX_CACHE_ENTRIES
            {
                break;
            }
            if evicted_key == key {
                continue;
            }
            let evict_url = cache_url(&evicted_key);
            wasm_bindgen_futures::JsFuture::from(cache.delete_with_str(&evict_url))
                .await
                .map_err(|e| format!("{e:?}"))?;
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
    if generation != STORAGE_GENERATION.load(portable_atomic::Ordering::Relaxed)
        || active_account() != Some(target_account)
    {
        return Ok(());
    }
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
    finish_meta(target_account, &meta)?;

    Ok(())
}

pub async fn delete_account_storage(
    account: Option<oxidezap_core::AccountId>,
) -> Result<(), String> {
    let Some(account) = account else {
        return Ok(());
    };
    STORAGE_GENERATION.fetch_add(1, portable_atomic::Ordering::Relaxed);
    let _origin_guard = super::web_lock::lock().await?;
    let stamp = persistent_stamp(Some(account))?
        .checked_add(1)
        .ok_or("cache generation exhausted")?;
    let storage = storage()?;
    storage
        .set_item(
            &format!("{}_generation", meta_storage_key(account)),
            &stamp.to_string(),
        )
        .map_err(|e| format!("{e:?}"))?;
    RECONCILED.with(|current| current.set(None));
    let caches = web_sys::window()
        .ok_or("no window")?
        .caches()
        .map_err(|e| format!("{e:?}"))?;
    wasm_bindgen_futures::JsFuture::from(caches.delete(&cache_name_for_account(account)))
        .await
        .map_err(|e| format!("{e:?}"))?;
    storage
        .remove_item(&meta_storage_key(account))
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

pub async fn clear_cache_storage(account: Option<oxidezap_core::AccountId>) -> Result<(), String> {
    delete_account_storage(account.or_else(active_account)).await
}

pub async fn avatar_cache_usage(account: oxidezap_core::AccountId) -> (u64, u64) {
    let Some(meta) = load_meta(account) else {
        return (0, 0);
    };
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
