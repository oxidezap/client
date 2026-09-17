//! Web implementation of persistent avatar storage via the browser Cache Storage API.
//!
//! Avatars survive browser refresh (F5) in the Cache Storage API (`window.caches`)
//! scoped to the current account. Keys are mapped to synthetic HTTP URLs
//! (`http://localhost/_oxidezap/avatar/<cache_key>`).
//!
//! Memory safety: Since the module is built with `--shared-memory`, browser APIs
//! cannot take direct views into wasm linear memory. Every byte slice crossing
//! into JS is copied into `js_sys::Uint8Array::from(...)`, and reads copy out
//! using `array.to_vec()`.

use wasm_bindgen::JsCast;

const SCOPE_STORAGE_KEY: &str = "oxidezap_avatar_scope";

pub fn account_scope() -> String {
    let Some(window) = web_sys::window() else {
        return "default".to_string();
    };
    let Ok(Some(storage)) = window.local_storage() else {
        return "default".to_string();
    };
    if let Ok(Some(scope)) = storage.get_item(SCOPE_STORAGE_KEY) {
        if !scope.is_empty() {
            return scope;
        }
    }
    let mut bytes = [0u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        return "default".to_string();
    }
    let mut hex = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut hex, "{:02x}", b);
    }
    let _ = storage.set_item(SCOPE_STORAGE_KEY, &hex);
    hex
}

pub fn rotate_account_scope() {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let mut bytes = [0u8; 16];
        if getrandom::getrandom(&mut bytes).is_ok() {
            let mut hex = String::with_capacity(32);
            for b in bytes {
                use std::fmt::Write;
                let _ = write!(&mut hex, "{:02x}", b);
            }
            let _ = storage.set_item(SCOPE_STORAGE_KEY, &hex);
        }
    }
}

pub fn cache_name() -> String {
    format!("oxidezap-avatar-v1-{}", account_scope())
}

pub fn cache_url(key: &str) -> String {
    format!("http://localhost/_oxidezap/avatar/{key}")
}

pub async fn read_persistent(key: &str) -> Option<Vec<u8>> {
    let window = web_sys::window()?;
    let caches = window.caches().ok()?;
    let cache_promise = caches.open(&cache_name());
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
    Some(array.to_vec())
}

pub async fn write_persistent(key: &str, bytes: &[u8]) -> Result<(), String> {
    let window = web_sys::window().ok_or_else(|| "no window".to_string())?;
    let caches = window.caches().map_err(|e| format!("{e:?}"))?;
    let cache_promise = caches.open(&cache_name());
    let cache_val = wasm_bindgen_futures::JsFuture::from(cache_promise)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let cache: web_sys::Cache = cache_val.dyn_into().map_err(|e| format!("{e:?}"))?;
    let url = cache_url(key);

    // Copy bytes into a JS-owned Uint8Array before passing across the wasm boundary
    let js_array = js_sys::Uint8Array::from(bytes);
    let response =
        web_sys::Response::new_with_opt_u8_array(Some(&js_array)).map_err(|e| format!("{e:?}"))?;
    let put_promise = cache.put_with_str(&url, &response);
    wasm_bindgen_futures::JsFuture::from(put_promise)
        .await
        .map_err(|e| format!("{e:?}"))?;
    Ok(())
}

pub async fn delete_account_storage() {
    let name = cache_name();
    if let Some(window) = web_sys::window() {
        if let Ok(caches) = window.caches() {
            let _ = wasm_bindgen_futures::JsFuture::from(caches.delete(&name)).await;
        }
    }
    rotate_account_scope();
}

pub async fn clear_cache_storage() {
    let name = cache_name();
    if let Some(window) = web_sys::window() {
        if let Ok(caches) = window.caches() {
            let _ = wasm_bindgen_futures::JsFuture::from(caches.delete(&name)).await;
        }
    }
}

pub fn spawn_task(fut: impl std::future::Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(fut);
}
