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
//!
//! The lock is the *page's*, not a caller's, and that is what the rest of
//! this file is about. A caller is a future somebody may drop — a Retry
//! replaces the task the attach runs in — so a grant handed back to one and
//! released when it goes is a lock this tab briefly holds against itself:
//! the next ask reaches the browser inside the release, `ifAvailable` answers
//! null, and the tab settles on "another tab is running this account" with no
//! other tab anywhere. Asking once per page and handing every caller the same
//! hold is what closes that window, and the two asks that would have raced
//! are the same ask.

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

thread_local! {
    /// This page's ask, and what came of it.
    ///
    /// One per page, because the lock is one per page. Present while an ask
    /// is outstanding and for as long as the grant it produced is alive;
    /// taken back out when the browser refuses, so a later attempt — the
    /// other tab having closed by then — asks again rather than repeating an
    /// answer that has expired.
    static ASK: RefCell<Option<Rc<RefCell<Ask>>>> = const { RefCell::new(None) };
}

/// One ask of the browser, and everyone waiting on it.
struct Ask {
    /// The grant, once there is one. Keeping it here is what holds the lock:
    /// the promise the browser waits on stays pending for as long as this
    /// does, and nothing in the page drops it.
    held: Option<Rc<Hold>>,
    /// Callers who arrived while the browser had not answered yet. None of
    /// them asked it anything.
    waiting: Vec<futures_channel::oneshot::Sender<Result<Rc<Hold>, String>>>,
}

/// The page's hold on the account.
///
/// Its whole substance is the sender: the lock is held for as long as the
/// promise the callback returned is pending, and dropping this settles it.
/// Which is why nothing here drops it — the browser releases the lock when
/// the agent goes, and that is the only release this page has.
struct Hold {
    _release: futures_channel::oneshot::Sender<()>,
}

/// Held for as long as the session is.
///
/// A token on the page's hold rather than the hold itself: letting go of one
/// is letting go of a caller, not of the account. See the module docs for why
/// those had to stop being the same thing.
pub(crate) struct Claim(#[expect(dead_code, reason = "a token; the Rc is the point")] Rc<Hold>);

/// Take the claim, or say who has it.
///
/// Asks the browser once per page. A caller arriving while that ask is
/// outstanding waits for its answer, and one arriving after a grant is handed
/// the grant.
///
/// # Errors
///
/// Another tab holds it, or the browser has no lock manager to ask — a very
/// old one, or a context without a `navigator`.
pub(crate) async fn take() -> Result<Claim, String> {
    // Already asked: either the answer is here, or it is on its way and this
    // caller joins the others waiting for it. Registered while the slot is
    // borrowed, so a grant cannot land between reading it and joining.
    let waiting = ASK.with(|cell| {
        let slot = cell.borrow();
        let ask = slot.as_ref()?;
        let mut ask = ask.borrow_mut();
        if let Some(held) = ask.held.clone() {
            return Some(Waiting::Held(held));
        }
        let (tell, told) = futures_channel::oneshot::channel();
        ask.waiting.push(tell);
        Some(Waiting::Told(told))
    });
    match waiting {
        Some(Waiting::Held(held)) => return Ok(Claim(held)),
        Some(Waiting::Told(told)) => {
            return match told.await {
                Ok(answer) => answer.map(Claim),
                Err(_) => Err("The browser did not answer the claim for this account.".to_string()),
            };
        }
        None => {}
    }

    let navigator = web_sys::window()
        .ok_or_else(|| "There is no window here to claim a session from.".to_string())?
        .navigator();
    let locks = navigator.locks();

    let (tell, told) = futures_channel::oneshot::channel();
    let ask = Rc::new(RefCell::new(Ask {
        held: None,
        waiting: vec![tell],
    }));
    ASK.with(|cell| *cell.borrow_mut() = Some(Rc::clone(&ask)));

    // Granted, refused, or never asked — three answers, because the third one
    // happens: `navigator.locks.request` can reject *before* it ever calls the
    // callback (a document that is not fully active, a browser that refuses
    // lock access at all), and the callback is the only thing that speaks
    // here. A rejection swallowed there leaves every waiter above holding a
    // sender nobody has, for the life of the page — and what the person sees
    // is a window that never finishes starting, with nothing in the console
    // and no retry.
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
        // and on a grant when the page goes.
        let _held = callback;
        if let Err(e) = JsFuture::from(request).await
            && let Some(tell) = rejected.borrow_mut().take()
        {
            let _ = tell.send(Answer::Failed(format!("{e:?}")));
        }
    });

    // The answer is settled here rather than in the caller, for the same
    // reason the closure is held there: a caller can be dropped, and a grant
    // that arrives after its asker has gone is still this page's grant. This
    // task is what installs it — so a Retry that replaces the attach mid-ask
    // finds the account already claimed by the page it is running in.
    wasm_bindgen_futures::spawn_local(async move {
        let answer = match was_granted.await {
            Ok(Answer::Granted) => Ok(Rc::new(Hold { _release: release })),
            Ok(Answer::Failed(detail)) => Err(format!(
                "The browser would not give this page a lock on the account: {detail}"
            )),
            Ok(Answer::Refused) => Err(
                // A sentence, because it is drawn as one: this is the whole
                // body text of the screen a second tab shows, not a fragment
                // appended after a colon.
                "Another tab is already running this account. Use that one, or \
                 close it and try again."
                    .to_string(),
            ),
            Err(_) => Err("The browser did not answer the claim for this account.".to_string()),
        };

        // A grant is kept, because it *is* the page's hold; anything else is
        // forgotten, so that a later attempt asks the browser again — the
        // other tab may have closed in the meantime, and a remembered refusal
        // would outlive the thing it described.
        let waiting = {
            let mut ask = ask.borrow_mut();
            match &answer {
                Ok(held) => ask.held = Some(Rc::clone(held)),
                Err(_) => ASK.with(|cell| *cell.borrow_mut() = None),
            }
            std::mem::take(&mut ask.waiting)
        };
        for tell in waiting {
            let _ = tell.send(answer.clone());
        }
    });

    match told.await {
        Ok(answer) => answer.map(Claim),
        Err(_) => Err("The browser did not answer the claim for this account.".to_string()),
    }
}

/// How a caller that did not do the asking gets its answer.
enum Waiting {
    /// The page already holds the lock.
    Held(Rc<Hold>),
    /// The ask is outstanding, and this is where its answer lands.
    Told(futures_channel::oneshot::Receiver<Result<Rc<Hold>, String>>),
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
