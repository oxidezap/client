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
}

impl Drop for Claim {
    /// Releases the lock, which is the only way it is ever released.
    ///
    /// The lock is held for as long as the promise the callback returned is
    /// pending, and this is what settles it — so letting go of a `Claim` is
    /// letting go of the account.
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

    // Granted, refused, or never asked — three answers, because the third one
    // happens: `navigator.locks.request` can reject *before* it ever calls the
    // callback (a document that is not fully active, a browser that refuses
    // lock access at all), and the callback is the only thing that speaks
    // here. A rejection swallowed there leaves this function waiting on a
    // sender the closure still holds, for the life of the page — and what the
    // person sees is a window that never finishes starting, with nothing in
    // the console and no retry.
    let (release, released) = futures_channel::oneshot::channel::<()>();
    let (granted, was_granted) = futures_channel::oneshot::channel::<Answer>();
    // Shared with the rejection path below, so whichever of the two happens
    // first is the one that answers and the other finds the sender gone.
    let granted = Rc::new(RefCell::new(Some(granted)));
    let rejected = Rc::clone(&granted);
    let released = Rc::new(RefCell::new(Some(released)));

    let callback = Closure::<dyn FnMut(wasm_bindgen::JsValue) -> js_sys::Promise>::new(
        move |lock: wasm_bindgen::JsValue| {
            let held = !lock.is_null() && !lock.is_undefined();
            if let Some(tell) = granted.borrow_mut().take() {
                let _ = tell.send(if held {
                    Answer::Granted
                } else {
                    Answer::Refused
                });
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
    // The request's promise settles when the callback's does, so on the happy
    // path the callback has already spoken by the time this resolves and the
    // sender is gone. What is watched for here is the other path: a rejection
    // that means the callback will never run at all.
    wasm_bindgen_futures::spawn_local(async move {
        // The callback lives here, and nowhere the caller can drop it.
        //
        // A `Closure` freed while the browser still holds a reference is a
        // panic rather than a missed call, and the browser holds this one
        // until the request's promise settles. Held by the granted `Claim`,
        // it was a caller's to lose: `retry_connection` replaces its task,
        // which drops the `attach` future this is awaited inside — two Retry
        // clicks before a rerender is enough — and the lock manager would
        // then call into a closure that had gone with it. This task is
        // detached and outlives every one of those, and it ends exactly when
        // the browser is finished: the promise settles on a refusal at once,
        // and on a grant when the `Claim` is dropped and the lock released.
        let _held = callback;
        if let Err(e) = JsFuture::from(request).await
            && let Some(tell) = rejected.borrow_mut().take()
        {
            let _ = tell.send(Answer::Failed(format!("{e:?}")));
        }
    });

    match was_granted.await {
        Ok(Answer::Granted) => Ok(Claim {
            release: Some(release),
        }),
        Ok(Answer::Failed(detail)) => Err(format!(
            "The browser would not give this page a lock on the account: {detail}"
        )),
        Ok(Answer::Refused) => Err(
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

/// What the browser said about the claim.
///
/// Three, not two. `Refused` is another tab holding it, which is an ordinary
/// answer with a screen of its own; `Failed` is the request itself rejecting,
/// which used to be no answer at all.
enum Answer {
    Granted,
    Refused,
    Failed(String),
}
