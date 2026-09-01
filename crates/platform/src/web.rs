//! The browser half: the loop the page already turns, and `setTimeout`.
//!
//! There is no runtime to build and no thread to build one on, so a task
//! spawned here never leaves the agent that spawned it — which is also why
//! nothing on this side is `Send`, and why it need not be: the `web-sys`
//! objects a task holds could not survive being moved anyway.
//!
//! The timer is the reason this crate exists. Bridging `setTimeout` into a
//! future is a dozen lines, which is exactly why it was written three times
//! and then diverged: one copy knew about workers and two did not, one
//! cancelled unconditionally and paid a `clearTimeout` per tick, and one
//! parked where another returned. It is written here once.

use std::cell::Cell;
use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

use crate::MaybeSend;

/// Hand `future` to the page's loop.
///
/// The page has one and `spawn_local` finds it from anywhere on the agent, so
/// unlike the desktop there is no runtime to name and nothing to attach to.
pub fn spawn(future: impl Future<Output = ()> + MaybeSend + 'static) {
    wasm_bindgen_futures::spawn_local(future);
}

/// Whichever global this agent has a `setTimeout` on.
///
/// A `Window` in the page and a `WorkerGlobalScope` in a worker. Both carry
/// the same two methods and neither inherits from the other, so the choice is
/// made once, here — and made at all, because this tree really does run
/// workers: rebuilding the standard library with atomics is what the whole
/// `build-std` dance is for, and a window-only timer is a wait that parks
/// forever the moment the code holding it is moved off the main agent.
enum Timers {
    Window(web_sys::Window),
    Worker(web_sys::WorkerGlobalScope),
}

thread_local! {
    /// The agent's global, resolved once.
    ///
    /// `web_sys::window()` is an `instanceof` across the wasm/JS boundary and
    /// the worker fallback is a second one, which is a strange price to pay
    /// per `setTimeout` on a path this hot: the library yields once per item
    /// it processes, so a history sync arms one of these per message.
    static TIMERS: Option<Timers> = Timers::here();
}

/// Do something with this agent's timers, if it has any.
fn with_timers<T>(f: impl FnOnce(&Timers) -> T) -> Option<T> {
    // `try_with` rather than `with`: a [`Timer`] can be dropped while thread
    // locals are being destroyed, and a panic there is a panic in a
    // destructor.
    TIMERS
        .try_with(|timers| timers.as_ref().map(f))
        .ok()
        .flatten()
}

impl Timers {
    fn here() -> Option<Self> {
        if let Some(window) = web_sys::window() {
            return Some(Self::Window(window));
        }
        js_sys::global()
            .dyn_into::<web_sys::WorkerGlobalScope>()
            .ok()
            .map(Self::Worker)
    }

    fn arm(&self, fire: &Closure<dyn FnMut()>, millis: i32) -> Result<i32, wasm_bindgen::JsValue> {
        match self {
            Self::Window(window) => window.set_timeout_with_callback_and_timeout_and_arguments_0(
                fire.as_ref().unchecked_ref(),
                millis,
            ),
            Self::Worker(worker) => worker.set_timeout_with_callback_and_timeout_and_arguments_0(
                fire.as_ref().unchecked_ref(),
                millis,
            ),
        }
    }

    fn disarm(&self, handle: i32) {
        match self {
            Self::Window(window) => window.clear_timeout_with_handle(handle),
            Self::Worker(worker) => worker.clear_timeout_with_handle(handle),
        }
    }
}

/// One armed `setTimeout`, disarmed when the wait is dropped.
///
/// Not tidiness: the `Closure` the browser would fire into is freed with this
/// struct, and calling a freed one is a wasm-bindgen panic rather than a
/// missed wakeup — and these waits are raced against something that routinely
/// wins.
struct Timer {
    handle: i32,
    /// Raised by the callback below. A timer that has already fired has
    /// nothing to cancel, and the ordinary end of a wait is exactly that — so
    /// cancelling unconditionally spends a `clearTimeout` across the boundary
    /// on every sleep, every retry and every yield the library makes, which on
    /// this target is one per item.
    fired: Rc<Cell<bool>>,
    _fire: Closure<dyn FnMut()>,
}

impl Drop for Timer {
    fn drop(&mut self) {
        if self.fired.get() {
            return;
        }
        with_timers(|timers| timers.disarm(self.handle));
    }
}

/// `setTimeout`, as a future. `false` when no timer could be armed.
///
/// A `false` is a wait that did not happen and is not coming: this agent has
/// no global with a `setTimeout` on it, or the browser refused the timer. The
/// caller decides what that means — see [`sleep`], which is what almost all of
/// them want.
pub async fn try_sleep(duration: Duration) -> bool {
    let (tx, rx) = futures_channel::oneshot::channel::<()>();
    let mut tx = Some(tx);
    let fired = Rc::new(Cell::new(false));
    let raise = Rc::clone(&fired);
    let fire = Closure::<dyn FnMut()>::new(move || {
        raise.set(true);
        if let Some(tx) = tx.take() {
            let _ = tx.send(());
        }
    });
    // Milliseconds, saturating: `setTimeout` takes an `i32`, and a wait longer
    // than 24 days is not a wait anything here asks for.
    let armed = with_timers(|timers| {
        timers.arm(
            &fire,
            i32::try_from(duration.as_millis()).unwrap_or(i32::MAX),
        )
    })
    .and_then(|handle| handle.ok())
    .map(|handle| Timer {
        handle,
        fired,
        _fire: fire,
    });
    let Some(_timer) = armed else {
        return false;
    };
    let _ = rx.await;
    true
}

/// Wait, and stop here if this agent has no timer.
///
/// Parking forever is the deliberate answer, and it is what the window's own
/// clock has always done: almost every caller of a sleep in this tree is a
/// loop that waits — a reconnect backoff, the QR rotation, a keepalive, a
/// repaint — so returning at once turns one into a spin that never yields and
/// takes the tab with it. Stopping the loop is the honest outcome and the log
/// is what says so.
///
/// The exception is a caller that is holding something for as long as it
/// waits, where parking leaks it for the life of the page; that one takes
/// [`try_sleep`] and answers for itself.
pub async fn sleep(duration: Duration) {
    if !try_sleep(duration).await {
        log::error!("this agent has no timer; whatever was waiting on one stops here");
        std::future::pending::<()>().await;
    }
}
