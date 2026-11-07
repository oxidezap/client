//! Browser I/O for the bounded catalog. No session data enters an asset URL.

use super::{Asset, CATALOG, Loader, Store, load};
use std::{borrow::Cow, cell::RefCell, rc::Rc, time::Duration};
use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;

const CACHE: &str = "oxidezap-script-fonts-v1";
const DEADLINE: Duration = Duration::from_secs(15);
const STORAGE_DEADLINE: Duration = Duration::from_secs(2);

pub(crate) fn install(cx: &mut gpui::App) {
    let loader = Rc::new(RefCell::new(Loader::default()));
    let reports = loader.clone();
    cx.on_missing_glyphs(move |glyphs, cx| {
        for glyph in glyphs {
            let id = reports.borrow_mut().claim(glyph.grapheme());
            if let Some(id) = id {
                start(id, reports.clone(), cx);
            }
        }
    })
    .detach();

    // A bounded recovery path independent of GPUI's deduplicated reports.
    // The listener and subscription live for the one application's page lifetime.
    let app = cx.to_async();
    let online = Closure::<dyn FnMut()>::new(move || {
        let ids = loader.borrow_mut().retry_failed();
        app.update(|cx| {
            for id in ids {
                start(id, loader.clone(), cx);
            }
        });
    });
    if let Some(window) = web_sys::window() {
        match window.add_event_listener_with_callback("online", online.as_ref().unchecked_ref()) {
            Ok(()) => online.forget(),
            Err(_) => {
                log::warn!("script font retries require a reload: online listener unavailable")
            }
        }
    }
}

fn start(id: usize, loader: Rc<RefCell<Loader>>, cx: &mut gpui::App) {
    cx.spawn(async move |cx| {
        let asset = &CATALOG[id];
        let result =
            match oxidezap_platform::with_timeout(load(&BrowserStore, asset), DEADLINE).await {
                Some(result) => result,
                None => Err("font load timed out".into()),
            };
        let result = cx.update(|cx| {
            let bytes = result?;
            cx.text_system()
                .add_fonts(vec![Cow::Owned(bytes)])
                .map_err(|_| "verified font could not be installed".to_string())?;
            // add_fonts invalidates font/line caches; wake retained visible rows too.
            cx.refresh_windows();
            Ok::<_, String>(())
        });
        loader.borrow_mut().complete(id, result.is_ok());
        if let Err(error) = result {
            log::warn!(
                "script font {} unavailable: {error}; reconnect or reload to retry",
                asset.file
            );
        }
    })
    .detach();
}

struct BrowserStore;

fn window() -> Result<web_sys::Window, String> {
    web_sys::window().ok_or_else(|| "font loading has no window".into())
}

fn url(asset: &Asset) -> Result<String, String> {
    // Location rather than document.baseURI: an HTML <base> must not retarget
    // font requests. Resolving a filename removes the page's query and fragment.
    let page = window()?.location().href().map_err(js)?;
    Ok(web_sys::Url::new_with_base(asset.file, &page)
        .map_err(js)?
        .href())
}

async fn cache() -> Result<web_sys::Cache, String> {
    let storage = window()?.caches().map_err(js)?;
    let cache: web_sys::Cache = JsFuture::from(storage.open(CACHE))
        .await
        .map_err(js)?
        .dyn_into()
        .map_err(js)?;
    // Only this bounded catalog belongs here. Retire old content-hashed versions
    // so upgrades do not accumulate unlimited font assets in this cache.
    let allowed = CATALOG.iter().map(url).collect::<Result<Vec<_>, _>>()?;
    let keys = JsFuture::from(cache.keys()).await.map_err(js)?;
    for key in js_sys::Array::from(&keys).iter() {
        let request: web_sys::Request = key.dyn_into().map_err(js)?;
        if !allowed.contains(&request.url()) {
            JsFuture::from(cache.delete_with_request(&request))
                .await
                .map_err(js)?;
        }
    }
    Ok(cache)
}

impl Store for BrowserStore {
    async fn cached(&self, asset: &Asset) -> Result<Option<Vec<u8>>, String> {
        let read = async {
            let cache = cache().await?;
            let response = JsFuture::from(cache.match_with_str(&url(asset)?))
                .await
                .map_err(js)?;
            if response.is_undefined() {
                return Ok(None);
            }
            let response = response.dyn_into::<web_sys::Response>().map_err(js)?;
            read_response(response, asset).await.map(Some)
        };
        oxidezap_platform::with_timeout(read, STORAGE_DEADLINE)
            .await
            .unwrap_or_else(|| Err("font cache read timed out".into()))
    }

    async fn remove(&self, asset: &Asset) {
        let remove = async {
            let cache = cache().await?;
            JsFuture::from(cache.delete_with_str(&url(asset)?))
                .await
                .map_err(js)?;
            Ok::<_, String>(())
        };
        let _ = oxidezap_platform::with_timeout(remove, STORAGE_DEADLINE).await;
    }

    async fn fetch(&self, asset: &Asset) -> Result<Vec<u8>, String> {
        let controller = web_sys::AbortController::new().map_err(js)?;
        let _abort = AbortOnDrop(controller.clone());
        let init = web_sys::RequestInit::new();
        init.set_mode(web_sys::RequestMode::SameOrigin);
        // Persistent caching is verified above; do not replay an HTTP-cached
        // error or corrupt response during an explicit recovery attempt.
        init.set_cache(web_sys::RequestCache::NoStore);
        init.set_redirect(web_sys::RequestRedirect::Error);
        init.set_signal(Some(&controller.signal()));
        let request = web_sys::Request::new_with_str_and_init(&url(asset)?, &init).map_err(js)?;
        let response = JsFuture::from(window()?.fetch_with_request(&request))
            .await
            .map_err(js)?;
        read_response(response.dyn_into().map_err(js)?, asset).await
    }

    async fn persist(&self, asset: &Asset, bytes: &[u8]) -> Result<(), String> {
        let write = async {
            let cache = cache().await?;
            // A JS-owned copy; browser APIs must not receive a view into shared WASM memory.
            let array = js_sys::Uint8Array::new_with_length(bytes.len() as u32);
            array.copy_from(bytes);
            let response = web_sys::Response::new_with_opt_js_u8_array(Some(&array)).map_err(js)?;
            JsFuture::from(cache.put_with_str(&url(asset)?, &response))
                .await
                .map_err(js)?;
            Ok(())
        };
        oxidezap_platform::with_timeout(write, STORAGE_DEADLINE)
            .await
            .unwrap_or_else(|| Err("font cache write timed out".into()))
    }
}

struct AbortOnDrop(web_sys::AbortController);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
struct CancelOnDrop(web_sys::ReadableStreamDefaultReader);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let cancel = self.0.cancel();
        oxidezap_platform::spawn(async move {
            let _ = JsFuture::from(cancel).await;
        });
    }
}

async fn read_response(response: web_sys::Response, asset: &Asset) -> Result<Vec<u8>, String> {
    let length = response.headers().get("content-length").map_err(js)?;
    asset.check_response(response.status(), length.as_deref())?;
    let body = response
        .body()
        .ok_or_else(|| "font response has no body".to_string())?;
    let reader = body
        .get_reader()
        .dyn_into::<web_sys::ReadableStreamDefaultReader>()
        .map_err(js)?;
    let reader = CancelOnDrop(reader);
    let mut bytes = Vec::with_capacity(asset.size);
    loop {
        let next = JsFuture::from(reader.0.read()).await.map_err(js)?;
        if js_sys::Reflect::get(&next, &JsValue::from_str("done"))
            .map_err(js)?
            .as_bool()
            == Some(true)
        {
            break;
        }
        let chunk = js_sys::Reflect::get(&next, &JsValue::from_str("value")).map_err(js)?;
        let chunk = chunk.dyn_into::<js_sys::Uint8Array>().map_err(js)?;
        let len = chunk.length() as usize;
        if len > asset.size - bytes.len() {
            return Err("font response exceeds catalog size".into());
        }
        let offset = bytes.len();
        bytes.resize(offset + len, 0);
        chunk.copy_to(&mut bytes[offset..]);
    }
    Ok(bytes)
}

fn js<T>(_error: T) -> String {
    // Avoid browser exception strings that may contain the page URL/token.
    "browser font storage or network operation failed".into()
}
