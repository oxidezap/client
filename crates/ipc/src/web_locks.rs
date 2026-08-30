//! A lock held for as long as its holder is alive, and a wait for one.
//!
//! The browser's `navigator.locks` is the only primitive in a page that
//! answers "is that other agent still there" without a heartbeat: a lock is
//! released when the agent holding it goes, whether it closed politely or was
//! killed, and a request queued behind it is granted at exactly that moment.
//!
//! `daemon/claim/` uses it for the account. This is the same API asked a
//! smaller question — one connection between two tabs, rather than one
//! session per user — and it lives here because both ends of that connection
//! need it: the follower holds, the leader waits. One implementation, in the
//! crate they already share.
//!
//! Nothing here uses `ifAvailable`. The claim does, because a refusal there is
//! an answer a person has to be shown; here a wait *is* the question.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::JsFuture;

/// A lock, held until this is dropped.
///
/// Or until the agent goes, which is the case this exists for: a tab that
/// crashes drops nothing, and the browser releases the lock anyway.
pub struct Hold {
    /// Dropping this settles the promise the lock manager is waiting on,
    /// which is the release. Nothing else here does anything.
    _release: futures_channel::oneshot::Sender<()>,
}

/// Take `name` and hold it.
///
/// Returns once the lock is granted, so a caller that needs the lock *before*
/// announcing itself can await this first.
///
/// # Errors
///
/// No lock manager to ask — a context with no `navigator`, or a browser old
/// enough to lack the API.
pub async fn hold(name: &str) -> Result<Hold, String> {
    let locks = manager()?;
    let (release, released) = futures_channel::oneshot::channel::<()>();
    let (granted, was_granted) = futures_channel::oneshot::channel::<()>();
    let granted = Rc::new(RefCell::new(Some(granted)));
    let rejected = Rc::clone(&granted);
    let released = Rc::new(RefCell::new(Some(released)));

    let callback = Closure::<dyn FnMut(wasm_bindgen::JsValue) -> js_sys::Promise>::new(
        move |_lock: wasm_bindgen::JsValue| {
            if let Some(tell) = granted.borrow_mut().take() {
                let _ = tell.send(());
            }
            let released = released.borrow_mut().take();
            wasm_bindgen_futures::future_to_promise(async move {
                if let Some(released) = released {
                    let _ = released.await;
                }
                Ok(wasm_bindgen::JsValue::UNDEFINED)
            })
        },
    );

    let request = locks.request(name, callback.as_ref().unchecked_ref());
    // The closure is held here rather than by the caller, and the task is
    // detached: a `Closure` freed while the lock manager still holds a
    // reference is a panic rather than a missed call, and a caller is a
    // future somebody may drop. See `daemon/claim/web.rs`, which learned this
    // the hard way.
    wasm_bindgen_futures::spawn_local(async move {
        let _held = callback;
        if let Err(e) = JsFuture::from(request).await
            && let Some(tell) = rejected.borrow_mut().take()
        {
            log::debug!("a lock request was refused outright: {e:?}");
            drop(tell);
        }
    });

    match was_granted.await {
        Ok(()) => Ok(Hold { _release: release }),
        Err(_) => Err(format!("the browser would not grant the lock {name}")),
    }
}

/// Wait until nobody holds `name`, then let it go again.
///
/// The whole of the answer is *when this returns*: the holder has gone. What
/// it does with the grant is release it immediately, because holding it would
/// make this the thing the next waiter waits on.
///
/// # Errors
///
/// No lock manager to ask, or a request the browser refused. Both are
/// permanent for this page, and a caller that cannot wait has to fall back on
/// something else rather than spin.
pub async fn wait_for(name: &str) -> Result<(), String> {
    drop(hold(name).await?);
    Ok(())
}

/// The browser's lock manager, in a window or in a worker.
fn manager() -> Result<web_sys::LockManager, String> {
    let global = js_sys::global();
    let navigator = js_sys::Reflect::get(&global, &wasm_bindgen::JsValue::from_str("navigator"))
        .map_err(|_| "there is no navigator here to take a lock from".to_string())?;
    let locks = js_sys::Reflect::get(&navigator, &wasm_bindgen::JsValue::from_str("locks"))
        .map_err(|_| "this browser has no lock manager".to_string())?;
    locks
        .dyn_into::<web_sys::LockManager>()
        .map_err(|_| "this browser has no lock manager".to_string())
}
