//! Waiting, where there is no reactor to wait on.
//!
//! The window waits for a lot of small things — a recording tick, a call's
//! second hand, a typing monitor, a pairing countdown, a frame of video — and
//! all of them were `smol::Timer::after`. That timer is backed by `async-io`,
//! which is an epoll loop: a page has none, and pulling one in drags `rustix`
//! and `errno` behind it, neither of which builds for a target whose OS is
//! "unknown".
//!
//! A browser's own timer is `setTimeout`, which is a callback rather than a
//! future. Bridging it is ten lines and no dependency, so that is what this
//! is — bound through `web-sys` in Rust like everything else here.

use std::time::Duration;

/// Wait, and come back.
///
/// Not a precise clock on either side: a browser clamps a background tab's
/// timers and a desktop timer is subject to the executor. Every caller here
/// is a repaint or a poll, which is exactly the kind of thing that may be
/// late.
pub async fn sleep(duration: Duration) {
    #[cfg(not(target_family = "wasm"))]
    {
        smol::Timer::after(duration).await;
    }
    #[cfg(target_family = "wasm")]
    {
        web::sleep(duration).await;
    }
}

/// Whichever finishes first: the work, or the wait.
///
/// `None` when the wait won. Written here rather than at the call site
/// because the two halves come from different places on the web — the future
/// is ours and the timer is the browser's — and racing them is the only thing
/// a caller wants.
pub async fn with_timeout<T>(work: impl Future<Output = T>, limit: Duration) -> Option<T> {
    let timeout = sleep(limit);
    futures_lite::future::or(async { Some(work.await) }, async {
        timeout.await;
        None
    })
    .await
}

#[cfg(target_family = "wasm")]
mod web {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::task::{Poll, Waker};
    use std::time::Duration;

    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::Closure;

    /// A `setTimeout` that has been armed, as a future.
    ///
    /// The closure has to outlive this call and be dropped when the timer
    /// fires or the future is dropped — which is why it is held here rather
    /// than forgotten: these are armed per tick, and a leaked closure per
    /// frame of video is a leak per frame of video.
    pub async fn sleep(duration: Duration) {
        let fired = Rc::new(RefCell::new(State::default()));
        let armed = fired.clone();
        let ring = Closure::<dyn FnMut()>::new(move || {
            let waker = {
                let mut state = armed.borrow_mut();
                state.fired = true;
                state.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        });

        // Milliseconds, saturating: `setTimeout` takes an `i32` and a wait
        // longer than 24 days is not a wait this window ever asks for.
        let millis = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
        let handle = web_sys::window().and_then(|window| {
            window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    ring.as_ref().unchecked_ref(),
                    millis,
                )
                .ok()
        });

        // Nothing to wait on: no window, or the browser refused the timer.
        //
        // Every caller here is a polling loop, so returning immediately would
        // turn one into a spin that never yields to the browser and freezes
        // the tab. Parking forever stops that loop instead, which is the
        // honest outcome: a page with no clock cannot animate anything, and
        // the loop had nothing left to wait for.
        if handle.is_none() {
            log::error!("this page has no timer; the loop that was waiting on one stops here");
            std::future::pending::<()>().await;
            return;
        }

        let _guard = Cancel {
            handle,
            _ring: ring,
        };
        std::future::poll_fn(|cx| {
            let mut state = fired.borrow_mut();
            if state.fired {
                Poll::Ready(())
            } else {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }

    #[derive(Default)]
    struct State {
        fired: bool,
        waker: Option<Waker>,
    }

    /// Clears the timer if the future is dropped before it fires, and keeps
    /// the closure alive until then. A timer whose callback has been freed is
    /// a crash rather than a missed wake.
    struct Cancel {
        handle: Option<i32>,
        _ring: Closure<dyn FnMut()>,
    }

    impl Drop for Cancel {
        fn drop(&mut self) {
            if let (Some(window), Some(handle)) = (web_sys::window(), self.handle) {
                window.clear_timeout_with_handle(handle);
            }
        }
    }
}
