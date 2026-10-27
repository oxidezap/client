//! Hold one origin-wide Web Lock until the Rust guard drops. A browser without
//! Web Locks keeps reading existing cache entries and bypasses persistent writes.

use std::cell::RefCell;
use std::rc::Rc;

use futures_channel::oneshot;
use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};

pub(super) struct Guard(Option<oneshot::Sender<()>>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            let _ = release.send(());
        }
    }
}

pub(super) async fn lock() -> Result<Guard, String> {
    let window = web_sys::window().ok_or("no window")?;
    let navigator = window.navigator();
    let locks = js_sys::Reflect::get(navigator.as_ref(), &"locks".into())
        .map_err(|_| "Web Locks unavailable")?;
    let request = js_sys::Reflect::get(&locks, &"request".into())
        .map_err(|_| "Web Locks unavailable")?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| "Web Locks unavailable")?;
    let (acquire, acquired) = oneshot::channel();
    let acquire = Rc::new(RefCell::new(Some(acquire)));
    let (release, released) = oneshot::channel();
    let mut released = Some(released);
    let callback_acquire = Rc::clone(&acquire);
    let callback = Closure::wrap(Box::new(move |_: JsValue| -> js_sys::Promise {
        if let Some(acquire) = callback_acquire.borrow_mut().take() {
            let _ = acquire.send(Ok(()));
        }
        let released = released.take();
        wasm_bindgen_futures::future_to_promise(async move {
            if let Some(released) = released {
                let _ = released.await;
            }
            Ok(JsValue::UNDEFINED)
        })
    }) as Box<dyn FnMut(JsValue) -> js_sys::Promise>);
    let result = request
        .call2(
            &locks,
            &"oxidezap-avatar-cache-v2".into(),
            callback.as_ref(),
        )
        .map_err(|_| "Web Lock request failed")?
        .dyn_into::<js_sys::Promise>()
        .map_err(|_| "Web Lock request was not a promise")?;
    // The callback must stay alive through both acquisition and release.
    oxidezap_platform::spawn(async move {
        let result = wasm_bindgen_futures::JsFuture::from(result).await;
        if let Some(acquire) = acquire.borrow_mut().take() {
            let _ = acquire.send(Err(format!(
                "Web Lock ended before acquisition: {result:?}"
            )));
        }
        drop(callback);
    });
    acquired
        .await
        .map_err(|_| "Web Lock acquisition canceled")??;
    Ok(Guard(Some(release)))
}
