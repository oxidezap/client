use anyhow::{Result, anyhow};
use wasm_bindgen::JsCast as _;
use wasm_bindgen_futures::JsFuture;

pub(super) async fn fetch(url: &str) -> Result<(u16, Vec<u8>)> {
    let window = web_sys::window().ok_or_else(|| anyhow!("no window to fetch avatar from"))?;
    let response = JsFuture::from(window.fetch_with_str(url))
        .await
        .map_err(|e| anyhow!("fetching avatar: {e:?}"))?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| anyhow!("avatar response was not a response"))?;
    let body = JsFuture::from(
        response
            .array_buffer()
            .map_err(|e| anyhow!("reading avatar body: {e:?}"))?,
    )
    .await
    .map_err(|e| anyhow!("reading avatar: {e:?}"))?;
    Ok((response.status(), js_sys::Uint8Array::new(&body).to_vec()))
}
