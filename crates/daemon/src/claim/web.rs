//! The claim a tab makes, through the browser's own lock.
//!
//! `navigator.locks` is scoped to the origin and outlives nothing: a tab that
//! goes away releases what it held, with no stale lock to reclaim and no
//! liveness check to write. That is the whole reason to use it rather than a
//! flag in storage, which is exactly the stale-entry problem a Unix socket's
//! `0700` directory has and answers with a peer uid.
//!
//! Taken with `ifAvailable`, so a second tab is told *now* rather than
//! queued. Queuing would be worse than refusing: a tab that sat waiting would
//! look like one that was starting, and the moment the first tab closed it
//! would silently take an account the person had stopped looking at.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::JsFuture;

/// The name the lock is held under.
///
/// One per origin, which is one per account: everything this protects — the
/// database, the media, the Signal state — is origin-scoped too.
const LOCK: &str = "oxidezap-session";

/// Held for as long as the session is.
///
/// Dropping it settles the promise the lock is held by, which is how the
/// browser is told. Nothing else releases it short of the tab going away,
/// which is the case this exists for.
pub(crate) struct Claim {
    /// Resolves the callback's promise, releasing the lock.
    release: Option<futures_channel::oneshot::Sender<()>>,
    /// The callback, kept alive because the browser still holds a reference
    /// to it. A `Closure` dropped while JS can still call it is a panic
    /// rather than a missed call.
    _held: Closure<dyn FnMut(wasm_bindgen::JsValue) -> js_sys::Promise>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

/// Take the claim, or say who has it.
///
/// # Errors
///
/// Another tab holds it, or the browser has no lock manager to ask — a very
/// old one, or a context without a `navigator`.
pub(crate) async fn take() -> Result<Claim, String> {
    let navigator = web_sys::window()
        .ok_or_else(|| "There is no window here to claim a session from.".to_string())?
        .navigator();
    let locks = navigator.locks();

    // Granted or refused, decided inside the callback: the lock exists only
    // for as long as the promise it returns is pending, so holding it means
    // handing back one that settles when this side lets go.
    let (release, released) = futures_channel::oneshot::channel::<()>();
    let (granted, was_granted) = futures_channel::oneshot::channel::<bool>();
    let granted = Rc::new(RefCell::new(Some(granted)));
    let released = Rc::new(RefCell::new(Some(released)));

    let callback = Closure::<dyn FnMut(wasm_bindgen::JsValue) -> js_sys::Promise>::new(
        move |lock: wasm_bindgen::JsValue| {
            let held = !lock.is_null() && !lock.is_undefined();
            if let Some(tell) = granted.borrow_mut().take() {
                let _ = tell.send(held);
            }
            let released = released.borrow_mut().take();
            wasm_bindgen_futures::future_to_promise(async move {
                // Refused: settle at once, or the browser waits on a promise
                // for a lock it never gave us.
                if !held {
                    return Ok(wasm_bindgen::JsValue::UNDEFINED);
                }
                if let Some(released) = released {
                    let _ = released.await;
                }
                Ok(wasm_bindgen::JsValue::UNDEFINED)
            })
        },
    );

    let options = web_sys::LockOptions::new();
    options.set_if_available(true);
    let request = locks.request_with_options(LOCK, &options, callback.as_ref().unchecked_ref());
    // The request's own promise settles when the callback's does, so it is
    // not what says whether we were granted; the callback is, and it has
    // already spoken by the time the browser has decided.
    wasm_bindgen_futures::spawn_local(async move {
        let _ = JsFuture::from(request).await;
    });

    match was_granted.await {
        Ok(true) => Ok(Claim {
            release: Some(release),
            _held: callback,
        }),
        Ok(false) => Err(
            // A sentence, because it is drawn as one: this is the whole body
            // text of the screen a second tab shows, not a fragment appended
            // after a colon.
            "Another tab is already running this account. Use that one, or \
             close it and try again."
                .to_string(),
        ),
        Err(_) => Err("The browser did not answer the claim for this account.".to_string()),
    }
}
