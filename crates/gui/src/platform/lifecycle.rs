//! Noticing that the front end is going away — and going, when asked to.
//!
//! A desktop `main` is the teardown: it waits for [`shutdown::requested`],
//! and everything after that wait disconnects the session and closes SQLite.
//! A page has no `main` that ends — the start function returns and the
//! browser owns the loop — so nothing there ever asked, and the ordered
//! teardown the daemon library carries was unreachable from a tab.
//!
//! [`shutdown::requested`]: oxidezap_daemon::shutdown::requested

use gpui::App;

/// Ask the platform to say when the front end is closing.
///
/// Called once, before the window opens. On a desktop it does nothing: the
/// process already has an ending, and adding a second route to it is the
/// mistake the shutdown module exists to prevent.
pub fn watch_for_departure() {
    imp::watch_for_departure();
}

/// Put the window away, because the tray asked.
///
/// What that means is the platform's. On a desktop the front end owns no
/// session and the window is the process, so going away is what closing the
/// window already does: the process ends, the daemon keeps the account, and
/// the tray's Open starts a fresh one — which is cheaper than it sounds, and
/// is the one thing a hidden window can be on Wayland, where a toplevel
/// cannot be withdrawn and brought back by its owner. A page cannot close
/// itself, and no tray reaches one anyway.
pub fn leave(cx: &mut App) {
    imp::leave(cx);
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    use gpui::App;

    /// `main` is the ending here, and it already waits.
    pub(super) fn watch_for_departure() {}

    /// The platform's own quit, which is what closing the last window
    /// reaches on every desktop but macOS — and there too, because a window
    /// that hid by merely closing would leave a process the daemon still
    /// counts as one, and Open would go on raising nothing.
    pub(super) fn leave(cx: &mut App) {
        cx.quit();
    }
}

#[cfg(target_family = "wasm")]
mod imp {
    use std::cell::RefCell;

    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::prelude::Closure;

    thread_local! {
        /// Held for the life of the page, which is exactly how long it is
        /// listening. Kept rather than forgotten so the listener can be
        /// described by something that owns it, and so a second call does
        /// not leave two.
        static LEAVING: RefCell<Option<Closure<dyn FnMut(web_sys::PageTransitionEvent)>>> =
            const { RefCell::new(None) };
    }

    /// A page cannot close itself — `window.close()` works only on a tab a
    /// script opened — and none is ever asked to: a socket client says it
    /// owns no window, and a page holding its own daemon has no tray.
    /// Answered here rather than left unmatched so the variant is handled
    /// wherever it can arrive.
    pub(super) fn leave(_cx: &mut gpui::App) {
        log::debug!("asked to hide, and a page has no window to put away");
    }

    pub(super) fn watch_for_departure() {
        let Some(window) = web_sys::window() else {
            return;
        };
        let leaving = Closure::<dyn FnMut(web_sys::PageTransitionEvent)>::new(
            move |event: web_sys::PageTransitionEvent| {
                // Not a page going into the back/forward cache. That one is
                // frozen and may be restored into the same session, so asking
                // it to shut down would end an account the person is about to
                // come back to.
                if event.persisted() {
                    return;
                }
                oxidezap_daemon::shutdown::request("the page is going away");
            },
        );
        if window
            .add_event_listener_with_callback("pagehide", leaving.as_ref().unchecked_ref())
            .is_err()
        {
            log::warn!("this page will not say when it is closing");
            return;
        }
        LEAVING.with(|held| *held.borrow_mut() = Some(leaving));
    }
}
